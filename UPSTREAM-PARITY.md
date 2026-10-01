# pi_rs ↔ upstream 方法级对比报告

> 基线：`upstream/` 检出于 `v0.99.2`（HEAD `005af57d8`）

> 方法：提取 TS 顶层导出（class/function/const/interface/type/enum）与 class 方法，
> 按 camelCase→snake_case 在 Rust 侧同名查找。`local` 命中视为 OK；
> `global`（同 crate 其他文件）记为「移位」；整 crate 无同名记 MISSING。
> 机械扫描，用于定位可疑缺口；命名差异、结构合并、宏生成（如 `tagged_error!`）、
> trait 默认实现会产生误报，需人工确认。**文末附人工逐项复核结论（豁免/适配/缺口三档）**。

## 总览

上游包 → Rust crate 映射（用于确认范围）：

- `packages/agent` → `crates/pi-agent`（**完整复刻目标**）
- `packages/ai` → `crates/pi-ai`（**声明子集**，范围见 `crates/pi-ai/AGENT.md`）
- `packages/telemetry` → `crates/pi-telemetry`
- 未复刻：`chord`、`client`、`codemode`、`coding-agent`、`durable`、`evals`、
  `mcp`、`protocol`、`server`、`session-backends`、`tui`（以及 agent 包内的 `pico3`、`polymarket`）

扫描统计：

- **agent** → `crates/pi-agent/src`：配对 83 · 缺文件 9 · 范围外 0 · 缺符号 139 · 移位 41
- **ai** → `crates/pi-ai/src`：配对 40 · 缺文件 0 · 范围外 151 · 缺符号 107 · 移位 5
- **telemetry** → `crates/pi-telemetry/src`：配对 6 · 缺文件 0 · 范围外 0 · 缺符号 12 · 移位 0

## packages/agent → `crates/pi-agent/src`

统计：配对 83 文件 · 文件缺失 9 · 范围外 0 · 符号缺失 139 · 符号移位 41

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

- `harness/session/testing/gating-storage.ts`
- `harness/session/testing/instrumented-storage.ts`
- `harness/session/testing/storage-decorator.ts`
- `harness/session/testing/benchmark/datasets.ts`
- `harness/session/testing/benchmark/session-repo.ts`
- `harness/session/testing/benchmark/storage.ts`
- `harness/session/testing/conformance/session-repo.ts`
- `harness/session/testing/conformance/storage.ts`
- `harness/session/jsonl/legacy-v3.ts`

### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）


**`agent.ts`** → `agent.rs`

- MISSING `AgentInitialState` (type) → 期望 `agent_initial_state`  ← 候选: `state`
- MISSING `continue` (method) → 期望 `continue`  ← 候选: `continue_turn`, `continue_operation`, `agent_loop_continue`, `run_agent_loop_continue`
- MISSING `followUpMode` (method) → 期望 `follow_up_mode`  ← 候选: `get_follow_up_mode`, `set_follow_up_mode`, `follow_up`
- MISSING `normalizePromptInput` (method) → 期望 `normalize_prompt_input`  ← 候选: `prompt`
- MISSING `steeringMode` (method) → 期望 `steering_mode`  ← 候选: `get_steering_mode`, `set_steering_mode`, `steer`
- <sub>移位 1 项：`prompt`</sub>

**`types.ts`** → `types.rs`

- MISSING `CustomAgentMessages` (type) → 期望 `custom_agent_messages`
- MISSING `FinishTurn` (type) → 期望 `finish_turn`  ← 候选: `finish`
- MISSING `PrepareRequest` (type) → 期望 `prepare_request`
- <sub>移位 1 项：`AgentToolCallOutcome`</sub>

**`harness/agent-harness.ts`** → `harness/agent-harness.rs`

- MISSING `AbortRequestResult` (type) → 期望 `abort_request_result`  ← 候选: `abort`
- MISSING `AcquireLaneOptions` (type) → 期望 `acquire_lane_options`
- MISSING `AgentHarness` (type) → 期望 `agent_harness`  ← 候选: `create_agent_harness`
- MISSING `AgentHarnessConstructor` (type) → 期望 `agent_harness_constructor`
- MISSING `AgentHarnessOptions` (type) → 期望 `agent_harness_options`
- MISSING `ConfigEventPayload` (type) → 期望 `config_event_payload`
- MISSING `EventListener` (type) → 期望 `event_listener`
- MISSING `Events` (type) → 期望 `events`  ← 候选: `process_events`, `committed_entry_events`, `entry_lifecycle_events`, `boundary_placement_events`
- MISSING `GlobalConfigEventPayload` (type) → 期望 `global_config_event_payload`
- MISSING `HandlerErrorPayload` (type) → 期望 `handler_error_payload`
- MISSING `HarnessEventPayload` (type) → 期望 `harness_event_payload`
- MISSING `HarnessEventType` (type) → 期望 `harness_event_type`  ← 候选: `event_type`
- MISSING `HookHandler` (type) → 期望 `hook_handler`
- MISSING `HookInvocation` (type) → 期望 `hook_invocation`
- MISSING `HookMap` (type) → 期望 `hook_map`
- MISSING `LaneConfigEventPayload` (type) → 期望 `lane_config_event_payload`  ← 候选: `lane_config`
- MISSING `LaneEventPayload` (type) → 期望 `lane_event_payload`
- MISSING `LaneExecutionInfo` (type) → 期望 `lane_execution_info`
- MISSING `LaneTranscriptSnapshot` (type) → 期望 `lane_transcript_snapshot`  ← 候选: `snapshot`
- MISSING `LaneWatchEvent` (type) → 期望 `lane_watch_event`  ← 候选: `watch`
- MISSING `OperationAdmissionError` (type) → 期望 `operation_admission_error`  ← 候选: `on_error`
- MISSING `OperationAdmissionResult` (type) → 期望 `operation_admission_result`
- MISSING `SpecialEventPayload` (type) → 期望 `special_event_payload`
- <sub>移位 7 项：`HarnessEvent`, `HookName`, `Hooks`, `LaneQueuedItem`, `LaneSnapshot`, `LaneSnapshotTool`, `SessionSnapshot`</sub>

**`harness/events.ts`** → `harness/events.rs`

- MISSING `enqueueBarrier` (method) → 期望 `enqueue_barrier`  ← 候选: `enqueue`
- MISSING `setUnsubscribe` (method) → 期望 `set_unsubscribe`  ← 候选: `unsubscribe`, `subscribe`
- MISSING `watchFromSnapshot` (method) → 期望 `watch_from_snapshot`  ← 候选: `snapshot`, `watch`

**`harness/hooks.ts`** → `harness/hooks.rs`

- MISSING `invokeToolRegistration` (method) → 期望 `invoke_tool_registration`

**`harness/result.ts`** → `harness/result.rs`

- MISSING `Closed` (class) → 期望 `closed`  ← 候选: `close`, `invoke_all_fail_closed`
- MISSING `HarnessClosed` (class) → 期望 `harness_closed`  ← 候选: `close`
- MISSING `HarnessFault` (class) → 期望 `harness_fault`  ← 候选: `fault`
- MISSING `InvalidLane` (class) → 期望 `invalid_lane`
- MISSING `InvalidMessage` (class) → 期望 `invalid_message`
- MISSING `InvalidNavigation` (class) → 期望 `invalid_navigation`
- MISSING `LaneBusy` (class) → 期望 `lane_busy`
- MISSING `NoActiveOperation` (class) → 期望 `no_active_operation`
- MISSING `NoActiveRun` (class) → 期望 `no_active_run`
- MISSING `NothingToCompact` (class) → 期望 `nothing_to_compact`  ← 候选: `compact`
- MISSING `NothingToResume` (class) → 期望 `nothing_to_resume`  ← 候选: `resume`
- MISSING `OperationMismatch` (class) → 期望 `operation_mismatch`  ← 候选: `mismatch`
- MISSING `Result` (type) → 期望 `result`  ← 候选: `get_result`, `_result_shape`, `result_entry_id`, `drive_run_result`
- MISSING `TaggedErrorFactory` (type) → 期望 `tagged_error_factory`
- MISSING `TaggedErrorValue` (type) → 期望 `tagged_error_value`  ← 候选: `value`
- MISSING `UnknownSkill` (class) → 期望 `unknown_skill`  ← 候选: `skill`
- MISSING `UnknownTarget` (class) → 期望 `unknown_target`
- MISSING `UnknownTemplate` (class) → 期望 `unknown_template`
- MISSING `is` (method) → 期望 `is`

**`harness/telemetry.ts`** → `harness/telemetry.rs`

- MISSING `AiSpan` (type) → 期望 `ai_span`  ← 候选: `start_ai_span`
- MISSING `AiSpanAttributes` (type) → 期望 `ai_span_attributes`
- MISSING `AiSpanEndAttributes` (type) → 期望 `ai_span_end_attributes`
- MISSING `AiSpanEventAttributes` (type) → 期望 `ai_span_event_attributes`
- MISSING `AiSpanEventName` (type) → 期望 `ai_span_event_name`
- MISSING `AiSpanName` (type) → 期望 `ai_span_name`
- MISSING `AiSpanStartAttributes` (type) → 期望 `ai_span_start_attributes`  ← 候选: `start`
- MISSING `AiTelemetrySpan` (type) → 期望 `ai_telemetry_span`
- MISSING `HarnessSpan` (type) → 期望 `harness_span`  ← 候选: `start_harness_span`
- MISSING `HarnessSpanAttributes` (type) → 期望 `harness_span_attributes`
- MISSING `HarnessSpanEndAttributes` (type) → 期望 `harness_span_end_attributes`
- MISSING `HarnessSpanEventAttributes` (type) → 期望 `harness_span_event_attributes`
- MISSING `HarnessSpanEventName` (type) → 期望 `harness_span_event_name`
- MISSING `HarnessSpanName` (type) → 期望 `harness_span_name`
- MISSING `HarnessSpanStartAttributes` (type) → 期望 `harness_span_start_attributes`  ← 候选: `start`
- MISSING `HarnessTelemetrySpan` (type) → 期望 `harness_telemetry_span`

**`harness/types.ts`** → `harness/types.rs`

- MISSING `AgentHarnessResources` (type) → 期望 `agent_harness_resources`
- MISSING `AgentHarnessToolContextSource` (type) → 期望 `agent_harness_tool_context_source`
- MISSING `Result` (type) → 期望 `result`  ← 候选: `get_result`, `_result_shape`, `result_entry_id`, `drive_run_result`
- MISSING `err` (fn) → 期望 `err`
- MISSING `ok` (fn) → 期望 `ok`
- MISSING `toError` (fn) → 期望 `to_error`
- <sub>移位 2 项：`getOrThrow`, `getOrUndefined`</sub>

**`harness/tools/bash.ts`** → `harness/tools/bash.rs`

- MISSING `BashToolDetails` (type) → 期望 `bash_tool_details`
- MISSING `BashToolInput` (type) → 期望 `bash_tool_input`

**`harness/tools/edit.ts`** → `harness/tools/edit.rs`

- MISSING `EditToolDetails` (type) → 期望 `edit_tool_details`
- MISSING `EditToolInput` (type) → 期望 `edit_tool_input`

**`harness/tools/read.ts`** → `harness/tools/read.rs`

- MISSING `ReadToolDetails` (type) → 期望 `read_tool_details`
- MISSING `ReadToolInput` (type) → 期望 `read_tool_input`

**`harness/tools/write.ts`** → `harness/tools/write.rs`

- MISSING `WriteToolInput` (type) → 期望 `write_tool_input`  ← 候选: `write`

**`harness/runtime/harness.ts`** → `harness/runtime/harness.rs`

- MISSING `getConfig` (method) → 期望 `get_config`
- MISSING `setConfig` (method) → 期望 `set_config`  ← 候选: `set_configuration_identity`

**`harness/runtime/lane.ts`** → `harness/runtime/lane.rs`

- MISSING `captureLaneSnapshot` (method) → 期望 `capture_lane_snapshot`  ← 候选: `capture_lane_snapshot_inner`, `snapshot`
- MISSING `requestOperationAbort` (method) → 期望 `request_operation_abort`  ← 候选: `abort`
- MISSING `setConfiguration` (method) → 期望 `set_configuration`  ← 候选: `set_configuration_identity`
- <sub>移位 1 项：`Lane`</sub>

**`harness/runtime/restore.ts`** → `harness/runtime/restore.rs`

- MISSING `restoreSession` (fn) → 期望 `restore_session`  ← 候选: `restore_session_arc`, `session`

**`harness/compaction/compaction.ts`** → `harness/compaction/compaction.rs`

- MISSING `SummaryGenerationOptions` (type) → 期望 `summary_generation_options`

**`harness/utils/adaptive-publisher.ts`** → `harness/utils/adaptive-publisher.rs`

- MISSING `AdaptivePublisherOptions` (type) → 期望 `adaptive_publisher_options`  ← 候选: `publish`

**`harness/utils/shell-output.ts`** → `harness/utils/shell-output.rs`

- MISSING `ShellCaptureOptions` (type) → 期望 `shell_capture_options`

**`harness/utils/truncate.ts`** → `harness/utils/truncate.rs`

- MISSING `utf8ByteLength` (fn) → 期望 `utf8_byte_length`

**`harness/session/commit.ts`** → `harness/session/commit.rs`

- MISSING `CommittedEntryWrite` (type) → 期望 `committed_entry_write`  ← 候选: `commit`, `write`
- MISSING `CommittedListAppendWrite` (type) → 期望 `committed_list_append_write`  ← 候选: `append`, `commit`, `write`
- MISSING `CommittedListDeleteWrite` (type) → 期望 `committed_list_delete_write`  ← 候选: `commit`, `delete`, `write`
- MISSING `CommittedUsageWrite` (type) → 期望 `committed_usage_write`  ← 候选: `commit`, `write`
- MISSING `CommittedValueDeleteWrite` (type) → 期望 `committed_value_delete_write`  ← 候选: `commit`, `delete`, `value`, `write`
- MISSING `CommittedValueSetWrite` (type) → 期望 `committed_value_set_write`  ← 候选: `commit`, `value`, `write`

**`harness/session/context.ts`** → `harness/session/context.rs`

- MISSING `SessionContextBuildOptions` (type) → 期望 `session_context_build_options`  ← 候选: `session`

**`harness/session/memory.ts`** → `harness/session/memory.rs`

- MISSING `MemorySessionRepoOptions` (type) → 期望 `memory_session_repo_options`  ← 候选: `session`
- MISSING `MemoryStorageOptions` (type) → 期望 `memory_storage_options`  ← 候选: `storage`
- MISSING `openRecord` (method) → 期望 `open_record`
- MISSING `reserveId` (method) → 期望 `reserve_id`
- MISSING `wrapBranch` (method) → 期望 `wrap_branch`  ← 候选: `branch`
- <sub>移位 16 项：`admit`, `appendList`, `beginMutation`, `branch`, `createBranch`, `deleteList`, `deleteValue`, `findEntries`, `findEntry`, `getEntry`, `getLabel`, `getName`, `mutate`, `setLabel`, `setName`, `setValue`</sub>

**`harness/session/session.ts`** → `harness/session/session.rs`

- MISSING `SessionBranchExistsError` (class) → 期望 `session_branch_exists_error`  ← 候选: `session`, `branch`, `exists`
- MISSING `SessionInvalidBranchError` (class) → 期望 `session_invalid_branch_error`  ← 候选: `session`, `branch`
- MISSING `SessionPendingAssistantMessageError` (class) → 期望 `session_pending_assistant_message_error`  ← 候选: `session`
- MISSING `SessionUnknownTargetError` (class) → 期望 `session_unknown_target_error`  ← 候选: `session`
- MISSING `StorageBackedSessionOptions` (type) → 期望 `storage_backed_session_options`  ← 候选: `session`, `storage`
- <sub>移位 2 项：`mutate`, `settle`</sub>

**`harness/session/types.ts`** → `harness/session/types.rs`

- MISSING `AssistantEffectPendingOperation` (type) → 期望 `assistant_effect_pending_operation`
- MISSING `AssistantReadyOperation` (type) → 期望 `assistant_ready_operation`
- MISSING `AssistantRetryWaitOperation` (type) → 期望 `assistant_retry_wait_operation`
- MISSING `CheckpointOperation` (type) → 期望 `checkpoint_operation`
- MISSING `DeferredEffectPendingOperation` (type) → 期望 `deferred_effect_pending_operation`
- MISSING `DeferredSuspendedOperation` (type) → 期望 `deferred_suspended_operation`
- MISSING `NavigationReadyToCommitOperation` (type) → 期望 `navigation_ready_to_commit_operation`  ← 候选: `commit`
- MISSING `OperationAt` (type) → 期望 `operation_at`
- MISSING `SessionMutator` (type) → 期望 `session_mutator`  ← 候选: `session`
- MISSING `StartingOperation` (type) → 期望 `starting_operation`  ← 候选: `start`
- MISSING `SummaryDecidingOperation` (type) → 期望 `summary_deciding_operation`
- MISSING `SummaryEffectPendingOperation` (type) → 期望 `summary_effect_pending_operation`
- MISSING `SummaryGenerationEffectPending` (type) → 期望 `summary_generation_effect_pending`
- MISSING `SummaryGenerationReady` (type) → 期望 `summary_generation_ready`
- MISSING `SummaryGenerationRetryWait` (type) → 期望 `summary_generation_retry_wait`
- MISSING `SummaryReadyOperation` (type) → 期望 `summary_ready_operation`
- MISSING `SummaryRetryWaitOperation` (type) → 期望 `summary_retry_wait_operation`
- MISSING `ToolsOperation` (type) → 期望 `tools_operation`
- MISSING `operationScopeOf` (fn) → 期望 `operation_scope_of`  ← 候选: `scope`

**`harness/session/testing/types.ts`** → `harness/session/testing/types.rs`

- MISSING `ConformanceCase` (type) → 期望 `conformance_case`
- MISSING `StorageFixture` (type) → 期望 `storage_fixture`  ← 候选: `storage`

**`harness/session/jsonl/storage.ts`** → `harness/session/jsonl/storage.rs`

- MISSING `applyCommit` (method) → 期望 `apply_commit`  ← 候选: `commit`
- MISSING `isLegacyV3` (method) → 期望 `is_legacy_v3`  ← 候选: `is_legacy_v3_session_header`
- MISSING `openLegacyV3` (method) → 期望 `open_legacy_v3`
- MISSING `openV4` (method) → 期望 `open_v4`
- MISSING `replayCommitted` (method) → 期望 `replay_committed`  ← 候选: `commit`
- MISSING `upgradeLegacyV3ToV4` (method) → 期望 `upgrade_legacy_v3_to_v4`
- MISSING `withImportedUsage` (method) → 期望 `with_imported_usage`

**`harness/session/jsonl/types.ts`** → `harness/session/jsonl/types.rs`

- MISSING `JsonlSessionRepoOptions` (type) → 期望 `jsonl_session_repo_options`  ← 候选: `session`
- <sub>移位 1 项：`JsonlStorageOptions`</sub>

## packages/ai → `crates/pi-ai/src`

统计：配对 40 文件 · 文件缺失 0 · 范围外 151 · 符号缺失 107 · 符号移位 5

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

（无）

<details><summary>范围外文件 151 个（AGENT.md 已声明不复刻）</summary>

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
- `providers/azure-openai-responses.models.ts`
- `providers/azure-openai-responses.ts`
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
- `api/azure-openai-responses.lazy.ts`
- `api/azure-openai-responses.ts`
- `api/bedrock-converse-stream.lazy.ts`
- `api/bedrock-converse-stream.ts`
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
- MISSING `AnthropicMessagesCompat` (type) → 期望 `anthropic_messages_compat`
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
- MISSING `ModelImageInputLimits` (type) → 期望 `model_image_input_limits`
- MISSING `ModelImageResizeOptions` (type) → 期望 `model_image_resize_options`
- MISSING `ModelInputLimits` (type) → 期望 `model_input_limits`
- MISSING `ModelPromptCache` (type) → 期望 `model_prompt_cache`  ← 候选: `prompt`
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
- MISSING `ProviderResponse` (type) → 期望 `provider_response`
- MISSING `ProviderStreamOptions` (type) → 期望 `provider_stream_options`
- MISSING `ProviderStreams` (type) → 期望 `provider_streams`
- MISSING `SessionAffinityFormat` (type) → 期望 `session_affinity_format`  ← 候选: `detect_session_affinity_format`
- MISSING `TextSignatureV1` (type) → 期望 `text_signature_v1`  ← 候选: `encode_text_signature_v1`
- MISSING `ThinkingTokenBudgetField` (type) → 期望 `thinking_token_budget_field`  ← 候选: `token`
- MISSING `VercelGatewayRouting` (type) → 期望 `vercel_gateway_routing`

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

- MISSING `TranscriptMessages` (type) → 期望 `transcript_messages`

**`api/constrained-sampling.ts`** → `api/constrained-sampling.rs`

- MISSING `UnsupportedStrictSchemaKeywordCheck` (type) → 期望 `unsupported_strict_schema_keyword_check`  ← 候选: `check`

**`api/openai-completions.ts`** → `api/openai-completions.rs`

- MISSING `ConvertCompletionsMessagesOptions` (type) → 期望 `convert_completions_messages_options`
- MISSING `OpenAICompletionsOptions` (type) → 期望 `open_ai_completions_options`
- MISSING `stream` (const) → 期望 `stream`  ← 候选: `stream_error`, `stream_simple`, `stream_request`, `stream_deferred`
- <sub>移位 2 项：`convertMessages`, `streamSimple`</sub>

**`api/openai-responses.ts`** → `api/openai-responses.rs`

- MISSING `OpenAIResponsesOptions` (type) → 期望 `open_ai_responses_options`
- MISSING `stream` (const) → 期望 `stream`  ← 候选: `stream_error`, `stream_simple`, `stream_request`, `stream_deferred`
- <sub>移位 1 项：`streamSimple`</sub>

## packages/telemetry → `crates/pi-telemetry/src`

统计：配对 6 文件 · 文件缺失 0 · 范围外 0 · 符号缺失 12 · 符号移位 0

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


---

# 本轮修复记录（2026-10-01，全量逻辑对齐）

以下 8 项已逐个对照上游源码核实并修复，`cargo check/clippy/test/fmt` 全绿：

1. **`AgentToolResult.isError` 传递丢失（逻辑 bug）**：`execute_prepared_tool_call` 原返回
   `AgentToolResult`，`finalize_executed_tool_call` 硬编码 `is_error = false`，导致工具抛错/panic 时
   `ToolResultMessage.isError` 仍为 false（模型会当成成功）。现新增 `ExecutedToolCallOutcome{result,is_error}`，
   对齐上游 `ExecutedToolCallOutcome`；`create_error_tool_result` 置 `is_error: true`。
2. **`AgentToolResult` 缺字段**：补 `structured_content`（对应 `structuredContent`）与 `is_error`
   （对应 `isError`），26 处构造点同步。
3. **`finalizeExecutedToolCall` 缺 `structuredContent` 联动**：上游「钩子只换 content 时丢弃旧
   structuredContent」的语义已补齐；`AfterToolCallResult` 同步补 `structured_content`。
4. **`tool_execution_update.partialResult` 降级**：原只传 `partial.details`（JSON），现改为完整
   `AgentToolResult`（对齐上游 `partialResult: any` 实际值）。
5. **`runToolCall` 公开入口缺失**：新增 `run_tool_call` / `RunToolCallOptions` / `ToolCallHooks` /
   `ToolUpdateSink` / `AgentToolCallOutcome`，并把 `prepare_tool_call`、`finalize_executed_tool_call`
   从 `&AgentLoopConfig` 解耦为 `&ToolCallHooks`（对齐上游签名）。已导出。
6. **`jsonl/storage.ts` torn 处理**：原用普通 `write_file`，改为 `publish_file_atomically`（上游实测
   原子发布，失败不再静默）。同时错误不再被丢弃。
7. **`session.ts` `SessionInvalidBranchError` 消息缺包装**：补齐上游
   `Invalid branch ${JSON.stringify(case)}: ${reason}` 外层格式。
8. **工具 `details` 丢失**：`bash` 现返回 `BashToolDetails{truncation,fullOutputPath}` 并补三种截断提示
   （含 `lastLinePartial` / `truncatedBy` 分支、`timeout`/`aborted` 错误文案、退出码消息顺序）；`read` 现返回
   `ReadToolDetails{truncation}`。`TruncationResult` 补 `Serialize`（camelCase）。

## 仍属“适配”而非缺口的项（本轮再次核实）

- **session 具名错误类型**：上游 4 个错误类（InvalidBranch/BranchExists/PendingAssistant/UnknownTarget）
  Rust 用 `Result<_, String>` 承载，**消息文本已逐字对齐**；上游自身无任何 `instanceof` 分支（已 grep 确认），
  故无行为差异，仅缺静态类型粒度。
- **`HarnessFault` / `HarnessClosed`**：lane 侧 `faulted` 语义已对齐（见上）；harness 对外仍统一
  `HarnessError::Closed`，上游唯一消费处（`lane.ts` 的 `instanceof HarnessFault`）已在 lane 层消化。
- **`events.ts` 三方法**：`enqueueBarrier` → Rust `delivery_tail` 互斥锁（`install_watcher` 内），
  `setUnsubscribe` → `unsubscribe_callback` + `watch_listeners` 重筛，`watchFromSnapshot` → `watch<T>()`。
  三者行为已逐一核对等价。
- **`jsonl/storage.ts` 私有方法**：`applyCommit` / `replayCommitted` 已在 `storage.rs::open`/`commit` 内联
  （validate→apply 两阶段完整）；`withImportedUsage` / `isLegacyV3` / `openLegacyV3` / `upgradeLegacyV3ToV4`
  属 legacy-v3（**用户已明确豁免**）。
- **`AgentToolResult.addedToolNames`**：Rust 有而上游 `AgentToolResult` 没有（上游在别处承载），非缺口。

## 未处理（完整台账见 `todos.md`；概要见 `UPSTREAM.md`「未完成清单」）

- **C1** 工具入参类型（`BashToolInput` / `EditToolInput` / `ReadToolInput` / `WriteToolInput`）
  —— 无运行时差异，仅缺 API 形态与编译期类型安全。〔小，可补〕
- **C2** `harness/telemetry.ts` 的 16 个 span 类型 vs Rust 的常量表 + schema JSON（无运行时行为）。〔小–中，可补〕
- **C3 / C4** session 具名错误、`HarnessFault` 变体 —— 已核实无行为差异，不建议投入。
- **B2 / C5** `harness/session/testing/` 下 8 个文件（gating/instrumented/storage-decorator +
  benchmark×3 + conformance×2）——纯测试基础设施；`conformance` 现有版本仍是「空 case 列表」。〔中–大〕
- **B1 / B3** 平台与迁移：Node 环境细节（已等价覆盖）、legacy-v3 JSONL（**用户明确不复刻**）。
- **上游已知偏差**：`onProviderStreamEvent` / Z.AI CN overflow / HTTP-date `Retry-After`。

---

# 人工复核结论（方法级 1:1 判定）

> 以下为对上方机械扫描结果的逐项核查结论。三档：**豁免**（语言/结构适配，非缺失）、
> **适配**（能力等价，命名或组织不同）、**缺口**（确认缺失或行为不等价）。

## A. 系统性误报（已确认豁免）

- **A1 `tagged_error!` 宏**：`harness/result.ts` 的 `LaneBusy`/`OperationMismatch`/`NoActiveRun`/
  `NoActiveOperation`/`NothingToResume`/`NothingToCompact`/`InvalidMessage`/`InvalidNavigation`/
  `UnknownSkill`/`UnknownTemplate`/`UnknownTarget`/`InvalidLane`/`Closed` 共 13 个错误类，
  Rust 侧由 `harness/result.rs` 的 `tagged_error!` 宏生成，**全部存在**。机械扫描未展开宏，误报。
- **A2 `Result` / `ok` / `err`**：`harness/types.ts` 的 `Result<T,E>`（tagged union）+ `ok()`/`err()`
  构造器，Rust 直接用标准库 `Result` 与 `Ok`/`Err`（`getOrThrow`→`get_or_throw`、
  `getOrUndefined`→`get_or_undefined` 已 1:1）。**豁免**。
- **A3 `*Operation` 接口**：`harness/session/types.ts` 的 `StartingOperation`/`CheckpointOperation`/
  `AssistantReadyOperation`/`ToolsOperation`/`SummaryReadyOperation`/`OperationAt` 等 16 个接口，
  Rust 对应 `harness/session/types.rs` 的 `OperationState` **enum variants** 与 `OperationState::scope()`。**豁免**。
- **A4 缩写命名**：`lazyOAuth`↔`lazy_oauth`、`pollOAuthDeviceCodeFlow`↔`poll_oauth_device_code_flow`
  等已由脚本的 compact 键（去分隔符小写）消除。
- **A5 pi-telemetry 12 项**：`InferEventAttributes`/`ExactTelemetryAttributes`/
  `TelemetrySchemaSpanUnion` 等全部是 TypeScript **类型级编程**（条件类型/映射类型/`UnionToIntersection`），
  Rust 无对应物也不需要。**豁免**。
- **A6 pi-ai 107 项**：绝大多数是 `Classifier*`/`Image*`/`*Compat`/`*Routing` 等
  **声明范围外**（见 `crates/pi-ai/AGENT.md`：只复刻 44 文件子集）。

## B. 命名/结构适配（能力等价）

- `AgentHarness`（interface + const 工厂） → Rust `AgentHarnessApi` + `Harness`
- `restoreSession` → `restore_session_arc`
- `captureLaneSnapshot`（private） → `capture_lane_snapshot_inner`
- `setConfiguration`（private） → `set_configuration_identity`
- `requestOperationAbort` → `request_abort`
- `invokeToolRegistration`（private） → `invoke_registration`
- `operationScopeOf` → `OperationState::scope()`
- `openRecord`/`reserveId`/`wrapBranch`（memory.ts private） → `MemorySessionRepo::open`/`create` 内联
- `getConfig`/`setConfig`（runtime/harness.ts private） → 直接 `self.config.lock()`
- `utf8ByteLength` → `str::len()`（Rust String 天然 UTF-8）
- `toError` → `drive/response.rs::normalize_error`
- `splitDeferredTools` → 已删除（对齐 v0.99.2）
- `HarnessFault` / `HarnessClosed` → 统一 `HarnessError::Closed`（**见 C2-2**）

## C. 真实缺口清单（基线；✅ = 本轮已修）

> 本节保留修复前的原始清单，便于对照。已修项见上方「本轮修复记录」。

### C1 文件级（pi-agent）

1. `harness/session/jsonl/legacy-v3.ts` — **用户已明确豁免**。
2. `harness/session/testing/{gating-storage,instrumented-storage,storage-decorator}.ts`
   — 测试用 storage 装饰器，Rust 侧未实现。
3. `harness/session/testing/benchmark/{datasets,session-repo,storage}.ts` — 基准测试设施，未实现。
4. `harness/session/testing/conformance/{session-repo,storage}.ts` — 契约测试；Rust 侧
   `testing/conformance.rs` 注明「返回空 conformance case 列表」（上游 1000+ 行）。

### C2 类型级（pi-agent）

1. session 具名错误 4 个（判定为适配，见上）。
2. **`HarnessFault` / `HarnessClosed`**：上游 `runtime/harness.ts` 用 `HarnessFault`（storage/invariant
   fault，带 `cause`）与 `HarnessClosed`（关闭时操作仍在跑）区分两类终止；`lane.ts` 依赖
   `closedError instanceof HarnessFault` 计算 `faulted` 标志。
   **本轮已修**：lane 新增 `SealReason::{Fault,Closed}`，`LaneSnapshot.faulted` 按「首次 seal 原因」
   取值（对齐上游 `??=` 语义）；`apply_fault` 传 `Fault`、`close` 传 `Closed`。
   残留：harness 对外仍统一返回 `HarnessError::Closed`（tag `Closed`），未建独立 `HarnessFault` 错误变体。
3. ✅ **工具入参/详情类型**：`BashToolDetails`/`ReadToolDetails` 已落地；`*ToolInput` 仍无同名导出
   （Rust 用 serde 内联反序列化）。
4. **Options 类型**：`AcquireLaneOptions`/`RunToolCallOptions`/`AgentToolCallOutcome`/
   `SummaryGenerationOptions`/`ShellCaptureOptions`/`AgentHarnessToolContextSource`/
   `AgentHarnessResources`/`StorageFixture` 无对应导出。
5. **`PrepareRequest` / `FinishTurn` / `CustomAgentMessages`**：上游 `harness/types.ts` 的类型；
   逻辑已随 v0.99.2 同步落地，但 Rust 侧没有同名公开类型。
6. **telemetry span 类型**：`harness/telemetry.ts` 的 `AiSpanName`/`AiSpanAttributes`/
   `AiSpanStartAttributes`/`HarnessSpanName`/`HarnessSpan*` 等 16 个类型。Rust 侧
   `harness/telemetry.rs` 只有 `HOOK_NAMES`/`EVENT_TYPES` 常量表 + `agent_telemetry_schemas()`，
   **无类型层**。

### C3 行为/API 级（pi-agent）

1. ✅ **`runToolCall`**：已新增公开 `run_tool_call` + `RunToolCallOptions`（见修复记录 5）。
2. **`events.ts` 三个方法**：行为已核对等价（见上「仍属适配」）。
3. ✅ **jsonl storage**：torn 已改为原子发布；`applyCommit` / `replayCommitted` 内联已核对等价。

### C4 已确认无需处理

- `onProviderStreamEvent`（0.99.0）：上游 provider 流层回调，本项目未用。
- overflow 的 Z.AI CN 端点检测：本项目未接入 Z.AI。
- HTTP-date 形式 `Retry-After`：Rust 侧走指数退避，上游用 `Date.parse`。
- telemetry：上游本版只改 CHANGELOG/package.json。
