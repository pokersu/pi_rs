//! Rust 翻译自 packages/ai/src/api/openai-responses.ts + openai-responses-shared.ts
//! （基础版：text + tool call 流式，省略 reasoning/deferred/custom-tool-call/service-tier）
//!
//! OpenAI Responses API（`POST /v1/responses`）的 SSE 流式调用，openai 与 deepseek 复用。

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{Value, json};

use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context,
    ErrorStopReason, ImageContent, InputModality, Model, SimpleStreamOptions, StopReason,
    StreamFunction, TerminalStopReason, TextContent, TextKind, TextOrImageContent, Tool, ToolCall,
    ToolCallKind, Usage,
};
use crate::utils::error_stream::{create_error_message, default_usage};
use crate::utils::event_stream::create_assistant_message_event_stream;
use crate::utils::json_parse::parse_streaming_json;

/// 对应 `OpenAIResponsesCompat`（从 `model.compat` JSON 解析）。
#[allow(dead_code)]
struct Compat {
    supports_developer_role: bool,
    session_affinity_format: String,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_openai_grammar_tools: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
    supports_explicit_prompt_cache_mode: bool,
}

/// 对应 `getCompat`。
fn get_compat(model: &Model) -> Compat {
    let compat = model.compat.as_ref().and_then(|c| c.as_object());
    let get_bool = |key: &str, default: bool| {
        compat
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    };
    Compat {
        supports_developer_role: get_bool("supportsDeveloperRole", true),
        session_affinity_format: compat
            .and_then(|c| c.get("sessionAffinityFormat"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| detect_session_affinity_format(model)),
        supports_long_cache_retention: get_bool("supportsLongCacheRetention", true),
        supports_strict_mode: get_bool("supportsStrictMode", false),
        supports_openai_grammar_tools: get_bool("supportsOpenAIGrammarTools", false),
        supports_additional_tools: get_bool("supportsAdditionalTools", false),
        supports_tool_search: get_bool("supportsToolSearch", false),
        supports_explicit_prompt_cache_mode: get_bool("supportsExplicitPromptCacheMode", false),
    }
}

/// 对应 `detectSessionAffinityFormat`。
fn detect_session_affinity_format(model: &Model) -> String {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        "openrouter".to_string()
    } else {
        "openai".to_string()
    }
}

/// 对应 `resolveCacheRetention`。
fn resolve_cache_retention(cache_retention: Option<CacheRetention>) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if std::env::var("PI_CACHE_RETENTION")
        .map(|v| v == "long")
        .unwrap_or(false)
    {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// 对应 `getPromptCacheRetention`。
fn get_prompt_cache_retention(
    compat: &Compat,
    cache_retention: CacheRetention,
) -> Option<&'static str> {
    if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        Some("24h")
    } else {
        None
    }
}

/// 对应 `clampOpenAIPromptCacheKey`（截断到 64 字符）。
fn clamp_prompt_cache_key(key: Option<&str>) -> Option<String> {
    key.map(|k| k.chars().take(64).collect())
}

/// 对应 `getServiceTierCostMultiplier`。
fn get_service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// 对应 `applyServiceTierPricing`。
fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model_id: &str) {
    let multiplier = get_service_tier_cost_multiplier(model_id, service_tier);
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

/// 对应 `splitDeferredTools`：把工具分成立即发送与延迟加载两组。
fn split_deferred_tools(context: &Context, enabled: bool) -> (Vec<Tool>, HashMap<String, Tool>) {
    let mut unique: HashMap<String, Tool> = HashMap::new();
    for tool in context.tools.as_deref().unwrap_or(&[]) {
        unique
            .entry(tool.name.clone())
            .or_insert_with(|| tool.clone());
    }
    if !enabled {
        return (unique.into_values().collect(), HashMap::new());
    }

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deferred_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in &context.messages {
        match message {
            crate::types::Message::Assistant(a) => {
                for block in &a.content {
                    if let ContentBlock::ToolCall(tc) = block {
                        used.insert(tc.name.clone());
                    }
                }
            }
            crate::types::Message::ToolResult(r) => {
                for name in r.added_tool_names.as_deref().unwrap_or(&[]) {
                    if !used.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = HashMap::new();
    for (name, tool) in unique {
        if deferred_names.contains(&name) {
            deferred.insert(name, tool);
        } else {
            immediate.push(tool);
        }
    }
    (immediate, deferred)
}

/// 对应 `OPENAI_TOOL_CALL_PROVIDERS`。
const OPENAI_TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode"];

/// 对应 `OPENAI_RESPONSES_MIN_OUTPUT_TOKENS`。
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;

/// 对应 `normalizeIdPart`。
fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let truncated: String = sanitized.chars().take(64).collect();
    truncated.trim_end_matches('_').to_string()
}

/// 对应 `buildForeignResponsesItemId`。
fn build_foreign_responses_item_id(item_id: &str) -> String {
    let normalized = format!("fc_{}", crate::utils::hash::short_hash(item_id));
    if normalized.chars().count() > 64 {
        normalized.chars().take(64).collect()
    } else {
        normalized
    }
}

/// 对应 `normalizeToolCallId`。
fn normalize_tool_call_id(id: &str, model: &Model, source: &AssistantMessage) -> String {
    if !OPENAI_TOOL_CALL_PROVIDERS.contains(&model.provider.as_str()) {
        return normalize_id_part(id);
    }
    if !id.contains('|') {
        return normalize_id_part(id);
    }
    let (call_id, item_id) = id.split_once('|').unwrap();
    let normalized_call_id = normalize_id_part(call_id);
    let is_foreign_tool_call = source.provider != model.provider || source.api != model.api;
    let mut normalized_item_id = if is_foreign_tool_call {
        build_foreign_responses_item_id(item_id)
    } else {
        normalize_id_part(item_id)
    };
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

/// 对应 `encodeTextSignatureV1`。
fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    let mut payload = json!({ "v": 1, "id": id });
    if let Some(phase) = phase {
        payload["phase"] = json!(phase);
    }
    payload.to_string()
}

/// 对应 `parseTextSignature`。
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature?;
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed.get("v").and_then(|v| v.as_i64()) == Some(1)
        && let Some(id) = parsed.get("id").and_then(|v| v.as_str())
    {
        let phase = parsed.get("phase").and_then(|v| v.as_str());
        let phase = match phase {
            Some("commentary") | Some("final_answer") => Some(phase.unwrap().to_string()),
            _ => None,
        };
        return Some((id.to_string(), phase));
    }
    // Fall through to legacy plain-string handling.
    Some((signature.to_string(), None))
}

/// 对应 `convertToolResultOutput`：toolResult 内容转字符串或 input_text/input_image 数组。
fn convert_tool_result_output(model: &Model, content: &[TextOrImageContent]) -> Value {
    let text_result: String = content
        .iter()
        .filter_map(|c| match c {
            TextOrImageContent::Text(t) => Some(t.text.as_str()),
            TextOrImageContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&ImageContent> = content
        .iter()
        .filter_map(|c| match c {
            TextOrImageContent::Image(i) => Some(i),
            TextOrImageContent::Text(_) => None,
        })
        .collect();
    let has_text = !text_result.is_empty();

    if images.is_empty() || !model.input.contains(&InputModality::Image) {
        let text = if has_text {
            text_result
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return json!(crate::utils::sanitize_unicode::sanitize_surrogates(&text));
    }

    let mut output: Vec<Value> = Vec::new();
    if has_text {
        output.push(json!({ "type": "input_text", "text": crate::utils::sanitize_unicode::sanitize_surrogates(&text_result) }));
    }
    for image in images {
        output.push(json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.data)
        }));
    }
    Value::Array(output)
}

/// 对应 `convertMessages`（system/user/assistant/toolResult → responses input，含延迟工具加载）。
fn convert_messages(
    context: &Context,
    model: &Model,
    compat: &Compat,
    deferred_tools: &HashMap<String, Tool>,
    deferred_tools_mode: Option<&str>,
    grammar_properties: &HashMap<String, String>,
) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut loaded_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let normalized_messages = crate::api::transform_messages::transform_messages(
        &context.messages,
        model,
        Some(&normalize_tool_call_id),
    );

    if let Some(system) = &context.system_prompt
        && !system.is_empty()
    {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        items.push(json!({ "role": role, "content": system }));
    }

    for (msg_index, msg) in normalized_messages.iter().enumerate() {
        match msg {
            crate::types::Message::User(u) => match &u.content {
                crate::types::UserContent::Text(t) => {
                    items.push(json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": t }]
                    }));
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|b| match b {
                            crate::types::TextOrImageContent::Text(t) => {
                                json!({ "type": "input_text", "text": t.text })
                            }
                            crate::types::TextOrImageContent::Image(i) => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", i.mime_type, i.data)
                            }),
                        })
                        .collect();
                    if !content.is_empty() {
                        items.push(json!({ "role": "user", "content": content }));
                    }
                }
            },
            crate::types::Message::Assistant(a) => {
                let is_same_provider_and_api = a.provider == model.provider && a.api == model.api;
                let is_same_model = is_same_provider_and_api && a.model == model.id;
                let is_different_model = is_same_provider_and_api && a.model != model.id;
                let mut text_block_index = 0usize;
                let mut assistant_items: Vec<Value> = Vec::new();

                for block in &a.content {
                    match block {
                        ContentBlock::Thinking(thinking) => {
                            if let Some(signature) = &thinking.thinking_signature
                                && let Ok(item) = serde_json::from_str::<Value>(signature)
                            {
                                assistant_items.push(item);
                            }
                        }
                        ContentBlock::Text(t) => {
                            let parsed = parse_text_signature(t.text_signature.as_deref());
                            let fallback_id = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let msg_id = match &parsed {
                                Some((id, _)) if !id.is_empty() => {
                                    if id.len() > 64 {
                                        format!("msg_{}", crate::utils::hash::short_hash(id))
                                    } else {
                                        id.clone()
                                    }
                                }
                                _ => fallback_id,
                            };
                            let phase = parsed.and_then(|(_, phase)| phase);
                            let mut obj = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": t.text, "annotations": [] }],
                                "status": "completed",
                                "id": msg_id
                            });
                            if let Some(phase) = phase {
                                obj["phase"] = json!(phase);
                            }
                            assistant_items.push(obj);
                        }
                        ContentBlock::ToolCall(tc) => {
                            let (call_id, item_id_raw) = tc
                                .id
                                .split_once('|')
                                .map_or((tc.id.as_str(), None), |(c, i)| (c, Some(i)));
                            let custom_input_property = grammar_properties.get(&tc.name);
                            let mut item_id = item_id_raw;
                            if (is_different_model && item_id.is_some_and(|i| i.starts_with("fc_")))
                                || (custom_input_property.is_none()
                                    && !item_id.is_some_and(|i| i.starts_with("fc_")))
                            {
                                item_id = None;
                            }
                            let can_replay_namespace =
                                is_same_model || deferred_tools.contains_key(&tc.name);
                            if let Some(input_property) = custom_input_property {
                                let input =
                                    crate::api::constrained_sampling::get_grammar_tool_input(
                                        &tc.name,
                                        &tc.arguments,
                                        input_property,
                                    )
                                    .unwrap_or_else(|e| panic!("{e}"));
                                let mut obj = json!({
                                    "type": "custom_tool_call",
                                    "call_id": call_id,
                                    "name": tc.name,
                                    "input": input
                                });
                                if let Some(item_id) = item_id {
                                    obj["id"] = json!(item_id);
                                }
                                if can_replay_namespace && let Some(namespace) = &tc.namespace {
                                    obj["namespace"] = json!(namespace);
                                }
                                assistant_items.push(obj);
                            } else {
                                let mut obj = json!({
                                    "type": "function_call",
                                    "call_id": call_id,
                                    "name": tc.name,
                                    "arguments": tc.arguments.to_string()
                                });
                                if let Some(item_id) = item_id {
                                    obj["id"] = json!(item_id);
                                }
                                if can_replay_namespace && let Some(namespace) = &tc.namespace {
                                    obj["namespace"] = json!(namespace);
                                }
                                assistant_items.push(obj);
                            }
                        }
                        _ => {}
                    }
                }

                if !assistant_items.is_empty() {
                    items.extend(assistant_items);
                }
            }
            crate::types::Message::ToolResult(r) => {
                let call_id = r.tool_call_id.split('|').next().unwrap_or(&r.tool_call_id);
                let output = convert_tool_result_output(model, &r.content);
                if grammar_properties.contains_key(&r.tool_name) {
                    items.push(json!({
                        "type": "custom_tool_call_output",
                        "call_id": call_id,
                        "output": output
                    }));
                } else {
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output
                    }));
                }

                // 延迟加载的工具（additional-tools / tool-search）。
                let mut loaded: Vec<Tool> = Vec::new();
                for name in r.added_tool_names.as_deref().unwrap_or(&[]) {
                    if let Some(tool) = deferred_tools.get(name)
                        && loaded_names.insert(name.clone())
                    {
                        loaded.push(tool.clone());
                    }
                }
                if !loaded.is_empty() && deferred_tools_mode == Some("additional-tools") {
                    items.push(json!({
                        "type": "additional_tools",
                        "role": "developer",
                        "tools": convert_tools(&loaded, compat)
                    }));
                } else if !loaded.is_empty() && deferred_tools_mode == Some("tool-search") {
                    let names: Vec<String> = loaded.iter().map(|t| t.name.clone()).collect();
                    let search_call_id = format!(
                        "pi_tool_load_{}",
                        crate::utils::hash::short_hash(&format!("{call_id}:{}", names.join(",")))
                    );
                    items.push(json!({
                        "type": "tool_search_call",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "arguments": { "query": names.join(" "), "limit": names.len() }
                    }));
                    items.push(json!({
                        "type": "tool_search_output",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "tools": convert_tools(&loaded, compat)
                    }));
                }
            }
        }
    }

    items
}

/// 对应 `convertTools`（function / custom grammar tool）。
fn convert_tools(tools: &[crate::types::Tool], compat: &Compat) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            if let Ok(Some(grammar)) =
                crate::api::constrained_sampling::resolve_grammar_constrained_sampling(
                    tool,
                    compat.supports_openai_grammar_tools,
                )
            {
                return json!({
                    "type": "custom",
                    "name": tool.name,
                    "description": tool.description,
                    "format": {
                        "type": "grammar",
                        "syntax": grammar.format,
                        "definition": grammar.definition
                    }
                });
            }
            let strict = crate::api::constrained_sampling::resolve_json_schema_strict_sampling(
                tool,
                compat.supports_strict_mode,
            )
            .unwrap_or_else(|e| panic!("{e}"));
            let parameters =
                crate::api::constrained_sampling::get_json_schema_tool_parameters(tool, strict);
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": parameters,
                "strict": strict.unwrap_or(false)
            })
        })
        .collect()
}

/// 对应 `buildParams`（含 compat / prompt cache / service tier / deferred tools）。
fn build_body(model: &Model, context: &Context, options: Option<&SimpleStreamOptions>) -> Value {
    let compat = get_compat(model);
    let deferred_tools_mode = if compat.supports_additional_tools {
        Some("additional-tools")
    } else if compat.supports_tool_search {
        Some("tool-search")
    } else {
        None
    };
    let (immediate_tools, deferred_tools) =
        split_deferred_tools(context, deferred_tools_mode.is_some());
    let grammar_properties = crate::api::constrained_sampling::create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_openai_grammar_tools,
    );

    let mut body = json!({
        "model": model.id,
        "input": convert_messages(
            context,
            model,
            &compat,
            &deferred_tools,
            deferred_tools_mode,
            &grammar_properties,
        ),
        "stream": true,
        "store": false,
    });

    if let Some(options) = options {
        let clamped_max_tokens = crate::api::simple_options::clamp_max_tokens_to_context(
            model,
            context,
            options.stream.max_tokens.unwrap_or(model.max_tokens),
        );
        body["max_output_tokens"] =
            json!(clamped_max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
        if let Some(temperature) = options.stream.temperature {
            body["temperature"] = json!(temperature);
        }

        let cache_retention = resolve_cache_retention(options.stream.cache_retention);
        if cache_retention != CacheRetention::None {
            body["prompt_cache_key"] =
                json!(clamp_prompt_cache_key(options.stream.session_id.as_deref()));
        }
        if let Some(retention) = get_prompt_cache_retention(&compat, cache_retention) {
            body["prompt_cache_retention"] = json!(retention);
        }
        if cache_retention == CacheRetention::None && compat.supports_explicit_prompt_cache_mode {
            body["prompt_cache_options"] = json!({ "mode": "explicit" });
        }

        // service tier（经 sampling_params 传入）。
        if let Some(service_tier) = options
            .stream
            .sampling_params
            .as_ref()
            .and_then(|s| s.get("service_tier"))
            .and_then(|v| v.as_str())
        {
            body["service_tier"] = json!(service_tier);
        }
    }

    if !immediate_tools.is_empty() {
        body["tools"] = json!(convert_tools(&immediate_tools, &compat));
    }

    body
}

/// 流式输出槽：对应 `ResponsesOutputSlot`。
enum Slot {
    Text {
        content_index: usize,
        text: String,
    },
    Thinking {
        content_index: usize,
    },
    ToolCall {
        content_index: usize,
        partial_json: String,
        arguments: serde_json::Value,
    },
}

struct StreamState {
    output: AssistantMessage,
    slots: HashMap<usize, Slot>,
    saw_terminal: bool,
    service_tier: Option<String>,
}

impl StreamState {
    fn new(model: &Model, service_tier: Option<String>) -> Self {
        Self {
            output: AssistantMessage {
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                usage: default_usage(),
                stop_reason: StopReason::Pending,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: crate::utils::uuid::now_ms() as u64,
            },
            slots: HashMap::new(),
            saw_terminal: false,
            service_tier,
        }
    }

    fn create_slot(
        &mut self,
        output_index: usize,
        item: &Value,
        stream: &crate::utils::event_stream::AssistantMessageEventStream,
    ) {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("reasoning") => {
                let content_index = self.output.content.len();
                self.output
                    .content
                    .push(ContentBlock::Thinking(crate::types::ThinkingContent {
                        kind: crate::types::ThinkingKind,
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                self.slots
                    .insert(output_index, Slot::Thinking { content_index });
                stream.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: self.output.clone(),
                });
            }
            Some("message") => {
                let content_index = self.output.content.len();
                self.output.content.push(ContentBlock::Text(TextContent {
                    kind: TextKind,
                    text: String::new(),
                    text_signature: None,
                }));
                self.slots.insert(
                    output_index,
                    Slot::Text {
                        content_index,
                        text: String::new(),
                    },
                );
                stream.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: self.output.clone(),
                });
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content_index = self.output.content.len();
                self.output.content.push(ContentBlock::ToolCall(ToolCall {
                    kind: ToolCallKind,
                    id: if item_id.is_empty() {
                        call_id.clone()
                    } else {
                        format!("{call_id}|{item_id}")
                    },
                    name,
                    arguments: serde_json::Value::Object(Default::default()),
                    thought_signature: None,
                    namespace: item
                        .get("namespace")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                }));
                self.slots.insert(
                    output_index,
                    Slot::ToolCall {
                        content_index,
                        partial_json: item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments: serde_json::Value::Object(Default::default()),
                    },
                );
                stream.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: self.output.clone(),
                });
            }
            Some("custom_tool_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = item.get("input").and_then(|v| v.as_str()).unwrap_or("");
                let content_index = self.output.content.len();
                self.output.content.push(ContentBlock::ToolCall(ToolCall {
                    kind: ToolCallKind,
                    id: if item_id.is_empty() {
                        call_id.clone()
                    } else {
                        format!("{call_id}|{item_id}")
                    },
                    name,
                    arguments: serde_json::json!({ "input": input }),
                    thought_signature: None,
                    namespace: item
                        .get("namespace")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                }));
                self.slots.insert(
                    output_index,
                    Slot::ToolCall {
                        content_index,
                        partial_json: input.to_string(),
                        arguments: serde_json::json!({ "input": input }),
                    },
                );
                stream.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: self.output.clone(),
                });
            }
            _ => {}
        }
    }

    fn push_text_delta(
        &mut self,
        output_index: usize,
        delta: &str,
        stream: &crate::utils::event_stream::AssistantMessageEventStream,
    ) {
        let Some(content_index) = self.text_content_index(output_index) else {
            return;
        };
        if let ContentBlock::Text(block) = &mut self.output.content[content_index] {
            block.text.push_str(delta);
        }
        stream.push(AssistantMessageEvent::TextDelta {
            content_index,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    fn push_toolcall_delta(
        &mut self,
        output_index: usize,
        delta: &str,
        stream: &crate::utils::event_stream::AssistantMessageEventStream,
    ) {
        let Some(content_index) = self.toolcall_content_index(output_index) else {
            return;
        };
        stream.push(AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    fn text_content_index(&self, output_index: usize) -> Option<usize> {
        match self.slots.get(&output_index) {
            Some(Slot::Text { content_index, .. }) => Some(*content_index),
            _ => None,
        }
    }

    fn toolcall_content_index(&self, output_index: usize) -> Option<usize> {
        match self.slots.get(&output_index) {
            Some(Slot::ToolCall { content_index, .. }) => Some(*content_index),
            _ => None,
        }
    }

    fn finalize_usage(&mut self, usage: &Value) {
        let input_tokens = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cached_tokens = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let reasoning_tokens = usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        self.output.usage = Usage {
            input: input_tokens.saturating_sub(cached_tokens),
            output: output_tokens,
            cache_read: cached_tokens,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: Some(reasoning_tokens),
            total_tokens: input_tokens + output_tokens,
            cost: crate::types::UsageCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.0,
            },
        };
    }

    fn finalize_stop_reason(&mut self, status: Option<&str>, incomplete_reason: Option<&str>) {
        let (stop_reason, error_message) = map_stop_reason(status, incomplete_reason);
        self.output.stop_reason = stop_reason;
        self.output.error_message = error_message;
        self.output.raw_stop_reason = status.map(|s| s.to_string());
        // 有 tool call 且 stop → toolUse。
        if self.output.stop_reason == StopReason::Stop
            && self
                .output
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall(_)))
        {
            self.output.stop_reason = StopReason::ToolUse;
        }
    }
}

/// 对应 `mapStopReason`。
fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> (StopReason, Option<String>) {
    match status {
        None => (StopReason::Stop, None),
        Some("completed") => (StopReason::Stop, None),
        Some("incomplete") => {
            if incomplete_reason == Some("max_output_tokens") {
                (StopReason::Length, None)
            } else {
                (
                    StopReason::Error,
                    Some(
                        incomplete_reason
                            .map(|r| format!("Response incomplete: {r}"))
                            .unwrap_or_else(|| {
                                "Response incomplete without a provider reason".to_string()
                            }),
                    ),
                )
            }
        }
        Some("failed") | Some("cancelled") => (StopReason::Error, None),
        Some("in_progress") | Some("queued") => (StopReason::Stop, None),
        _ => (StopReason::Stop, None),
    }
}

async fn stream_request(
    base_url: &str,
    api_key: &str,
    model: &Model,
    body: Value,
    service_tier: Option<String>,
    stream: &crate::utils::event_stream::AssistantMessageEventStream,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {text}"));
    }

    let mut state = StreamState::new(model, service_tier);
    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut data_line = String::new();

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..=newline).collect();
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() {
                // 空行分隔一个 SSE 事件。
                if !data_line.is_empty() {
                    process_event(&data_line, &mut state, stream)?;
                    data_line.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.trim();
                if data == "[DONE]" {
                    break;
                }
                data_line = data.to_string();
            }
            // `event:` 行忽略，type 在 data JSON 里。
        }
    }

    if !state.saw_terminal {
        return Err("OpenAI Responses stream ended before a terminal response event".to_string());
    }

    let reason = match state.output.stop_reason {
        StopReason::Stop => TerminalStopReason::Stop,
        StopReason::Length => TerminalStopReason::Length,
        StopReason::ToolUse => TerminalStopReason::ToolUse,
        StopReason::Deferred => TerminalStopReason::Deferred,
        _ => TerminalStopReason::Stop,
    };
    stream.push(AssistantMessageEvent::Done {
        reason,
        message: state.output.clone(),
    });
    stream.end(Some(state.output));
    Ok(())
}

/// 处理单个 SSE 事件（data JSON），映射到 AssistantMessageEvent。
fn process_event(
    data: &str,
    state: &mut StreamState,
    stream: &crate::utils::event_stream::AssistantMessageEventStream,
) -> Result<(), String> {
    let event: Value = serde_json::from_str(data).map_err(|e| format!("SSE JSON 解析失败: {e}"))?;
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "response.created" => {
            if let Some(id) = event
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
            {
                state.output.response_id = Some(id.to_string());
            }
        }
        "response.output_item.added" => {
            if let Some(output_index) = event.get("output_index").and_then(|v| v.as_u64())
                && let Some(item) = event.get("item")
            {
                state.create_slot(output_index as usize, item, stream);
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(output_index) = event.get("output_index").and_then(|v| v.as_u64()) {
                let delta = event.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                let output_index = output_index as usize;
                if let Some(Slot::Thinking { content_index }) = state.slots.get(&output_index) {
                    let content_index = *content_index;
                    if let ContentBlock::Thinking(block) = &mut state.output.content[content_index]
                    {
                        block.thinking.push_str(delta);
                    }
                    stream.push(AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: delta.to_string(),
                        partial: state.output.clone(),
                    });
                }
            }
        }
        "response.reasoning_summary_part.done" => {
            if let Some(output_index) = event.get("output_index").and_then(|v| v.as_u64()) {
                let output_index = output_index as usize;
                if let Some(Slot::Thinking { content_index }) = state.slots.get(&output_index) {
                    let content_index = *content_index;
                    if let ContentBlock::Thinking(block) = &mut state.output.content[content_index]
                    {
                        block.thinking.push_str("\n\n");
                    }
                    stream.push(AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: "\n\n".to_string(),
                        partial: state.output.clone(),
                    });
                }
            }
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            if let Some(output_index) = event.get("output_index").and_then(|v| v.as_u64()) {
                let delta = event.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                state.push_text_delta(output_index as usize, delta, stream);
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(output_index) = event.get("output_index").and_then(|v| v.as_u64()) {
                let delta = event.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                let output_index = output_index as usize;
                if let Some(Slot::ToolCall {
                    partial_json,
                    arguments,
                    ..
                }) = state.slots.get_mut(&output_index)
                {
                    partial_json.push_str(delta);
                    *arguments = parse_streaming_json(Some(partial_json));
                }
                state.push_toolcall_delta(output_index, delta, stream);
            }
        }
        "response.function_call_arguments.done" => {
            if let Some(output_index) = event.get("output_index").and_then(|v| v.as_u64()) {
                let output_index = output_index as usize;
                if let Some(Slot::ToolCall {
                    partial_json,
                    arguments,
                    ..
                }) = state.slots.get_mut(&output_index)
                {
                    let full = event
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let delta = full.strip_prefix(partial_json.as_str()).unwrap_or(full);
                    *partial_json = full.to_string();
                    *arguments = parse_streaming_json(Some(partial_json));
                    if !delta.is_empty() {
                        state.push_toolcall_delta(output_index, delta, stream);
                    }
                }
            }
        }
        "response.output_item.done" => {
            let Some(item) = event.get("item") else {
                return Ok(());
            };
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let output_index = event
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            match item_type {
                "reasoning" => {
                    if let Some(Slot::Thinking { content_index }) =
                        state.slots.remove(&output_index)
                    {
                        let summary_text = item
                            .get("summary")
                            .and_then(|s| s.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .map(|c| c.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let content_text = item
                            .get("content")
                            .and_then(|c| c.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .map(|c| c.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let accumulated = if let ContentBlock::Thinking(block) =
                            &state.output.content[content_index]
                        {
                            block.thinking.clone()
                        } else {
                            String::new()
                        };
                        let final_thinking = if !summary_text.is_empty() {
                            summary_text
                        } else if !content_text.is_empty() {
                            content_text
                        } else {
                            accumulated
                        };
                        if let ContentBlock::Thinking(block) =
                            &mut state.output.content[content_index]
                        {
                            block.thinking = final_thinking.clone();
                            block.thinking_signature = Some(item.to_string());
                        }
                        stream.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content: final_thinking,
                            partial: state.output.clone(),
                        });
                    }
                }
                "message" => {
                    if let Some(Slot::Text {
                        content_index,
                        text,
                    }) = state.slots.remove(&output_index)
                    {
                        let final_text = item
                            .get("content")
                            .and_then(|c| c.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .map(|c| {
                                        if c.get("type").and_then(|t| t.as_str())
                                            == Some("output_text")
                                        {
                                            c.get("text").and_then(|t| t.as_str()).unwrap_or("")
                                        } else {
                                            c.get("refusal").and_then(|t| t.as_str()).unwrap_or("")
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_else(|| text.clone());
                        if let ContentBlock::Text(block) = &mut state.output.content[content_index]
                        {
                            block.text = final_text.clone();
                            let phase = item.get("phase").and_then(|v| v.as_str());
                            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            block.text_signature = Some(encode_text_signature_v1(id, phase));
                        }
                        stream.push(AssistantMessageEvent::TextEnd {
                            content_index,
                            content: final_text,
                            partial: state.output.clone(),
                        });
                    }
                }
                "function_call" => {
                    if let Some(Slot::ToolCall {
                        content_index,
                        arguments,
                        ..
                    }) = state.slots.remove(&output_index)
                    {
                        let final_args = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .map(|s| parse_streaming_json(Some(s)))
                            .unwrap_or(arguments);
                        if let ContentBlock::ToolCall(block) =
                            &mut state.output.content[content_index]
                        {
                            block.arguments = final_args;
                        }
                        if let ContentBlock::ToolCall(block) = &state.output.content[content_index]
                        {
                            stream.push(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call: block.clone(),
                                partial: state.output.clone(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        "response.completed" | "response.incomplete" => {
            let response = event.get("response");
            let status = response
                .and_then(|r| r.get("status"))
                .and_then(|v| v.as_str());
            let incomplete_reason = response
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str());
            if let Some(usage) = response.and_then(|r| r.get("usage")) {
                state.finalize_usage(usage);
            }
            let service_tier = response
                .and_then(|r| r.get("service_tier"))
                .and_then(|v| v.as_str())
                .or(state.service_tier.as_deref());
            apply_service_tier_pricing(&mut state.output.usage, service_tier, &state.output.model);
            state.finalize_stop_reason(status, incomplete_reason);
            state.saw_terminal = true;
        }
        "response.failed" => {
            let response = event.get("response");
            let error = response.and_then(|r| r.get("error"));
            let msg = if let Some(error) = error {
                let code = error
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no message");
                format!("{code}: {message}")
            } else {
                "Unknown error (no error details in response)".to_string()
            };
            return Err(msg);
        }
        "error" => {
            let code = event
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let message = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!("Error Code {code}: {message}"));
        }
        _ => {}
    }

    Ok(())
}

/// 构造一个 OpenAI Responses 兼容的 stream 函数。
pub fn openai_responses_stream(base_url: String) -> StreamFunction {
    Arc::new(move |model, context, options| {
        let outer = create_assistant_message_event_stream();
        let producer = outer.clone();
        let base_url = base_url.clone();
        let model = model.clone();
        let body = build_body(&model, context, options);
        let api_key = options.and_then(|o| o.stream.request.api_key.clone());
        let service_tier = options
            .and_then(|o| o.stream.sampling_params.as_ref())
            .and_then(|s| s.get("service_tier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let provider = model.provider.clone();
        tokio::spawn(async move {
            match api_key {
                None => {
                    let error = create_error_message(
                        &format!("Missing API key for provider: {provider}"),
                        &model.api,
                        &model.provider,
                        &model.id,
                    );
                    producer.push(AssistantMessageEvent::Error {
                        reason: ErrorStopReason::Error,
                        error: error.clone(),
                    });
                    producer.end(Some(error));
                }
                Some(key) => {
                    if let Err(err) =
                        stream_request(&base_url, &key, &model, body, service_tier, &producer).await
                    {
                        let error =
                            create_error_message(&err, &model.api, &model.provider, &model.id);
                        producer.push(AssistantMessageEvent::Error {
                            reason: ErrorStopReason::Error,
                            error: error.clone(),
                        });
                        producer.end(Some(error));
                    }
                }
            }
        });
        outer
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        InputModality, Message, ModelCost, ModelCostRates, ToolResultMessage, UserContent,
        UserMessage,
    };

    fn make_model() -> Model {
        Model {
            id: "deepseek-chat".into(),
            name: "deepseek-chat".into(),
            api: "openai-responses".into(),
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost {
                rates: ModelCostRates {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.5,
                    cache_write: 1.0,
                },
                tiers: None,
            },
            context_window: 64000,
            max_tokens: 8192,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn assistant_with_tool_call(call_id: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                kind: ToolCallKind,
                id: call_id.to_string(),
                name: "github_get_pr".to_string(),
                arguments: serde_json::json!({"owner": "x", "repo": "y"}),
                thought_signature: None,
                namespace: None,
            })],
            api: "openai-responses".into(),
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            response_model: None,
            response_id: None,
            usage: default_usage(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        }
    }

    fn tool_result_for(call_id: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: call_id.to_string(),
            tool_name: "github_get_pr".to_string(),
            content: vec![TextOrImageContent::Text(TextContent {
                kind: TextKind,
                text: "result".to_string(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 2,
        }
    }

    fn convert_and_dump(messages: Vec<Message>) -> Vec<Value> {
        let model = make_model();
        let compat = get_compat(&model);
        let context = Context {
            system_prompt: None,
            messages,
            tools: None,
        };
        let items = convert_messages(
            &context,
            &model,
            &compat,
            &HashMap::new(),
            None,
            &HashMap::new(),
        );
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
        items
    }

    fn collect_call_ids(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| i.get("call_id").and_then(|v| v.as_str()).map(String::from))
            .collect()
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    #[test]
    fn tool_roundtrip_call_and_output_pair() {
        let items = convert_and_dump(vec![
            user("hi"),
            Message::Assistant(assistant_with_tool_call("call_00_X|fc_1")),
            Message::ToolResult(tool_result_for("call_00_X|fc_1")),
        ]);
        let call_ids = collect_call_ids(&items);
        assert_eq!(
            call_ids
                .iter()
                .filter(|c| c.as_str() == "call_00_X")
                .count(),
            2,
            "call + output 配对应恰好 2 次"
        );
    }

    #[test]
    fn orphan_tool_call_gets_single_synthetic_result() {
        let items = convert_and_dump(vec![
            user("hi"),
            Message::Assistant(assistant_with_tool_call("call_00_X|fc_1")),
        ]);
        let call_ids = collect_call_ids(&items);
        assert_eq!(
            call_ids
                .iter()
                .filter(|c| c.as_str() == "call_00_X")
                .count(),
            2,
            "孤儿 toolCall 合成一个 result，call + output 恰好 2 次"
        );
    }

    #[test]
    fn two_turns_with_distinct_call_ids() {
        // 两个 turn 的工具往返，call_id 不同。
        let items = convert_and_dump(vec![
            user("hi"),
            Message::Assistant(assistant_with_tool_call("call_00_A|fc_1")),
            Message::ToolResult(tool_result_for("call_00_A|fc_1")),
            Message::Assistant(assistant_with_tool_call("call_00_B|fc_2")),
            Message::ToolResult(tool_result_for("call_00_B|fc_2")),
        ]);
        let call_ids = collect_call_ids(&items);
        assert_eq!(
            call_ids
                .iter()
                .filter(|c| c.as_str() == "call_00_A")
                .count(),
            2
        );
        assert_eq!(
            call_ids
                .iter()
                .filter(|c| c.as_str() == "call_00_B")
                .count(),
            2
        );
    }
}
