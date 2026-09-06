//! Rust 翻译自 packages/agent/src/harness/execution/assistant.ts
//!
//! 一次已获批 assistant provider 请求的流式消费原语。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Message, Model,
    ModelThinkingLevel, ProviderRequestOptions, SimpleStreamOptions, StreamOptions, ThinkingLevel,
    Tool,
};

use crate::harness::context::Context;
use crate::harness::session::types::SettledAssistantMessage;
use crate::harness::types::AgentHarnessStreamOptions;
use crate::types::AgentMessage;

/// 对应 `AssistantResponseMetadata`：provider 响应体消费前捕获的 HTTP 元数据。
#[derive(Default, Clone)]
pub struct AssistantResponseMetadata {
    pub status: Option<u16>,
    pub headers: Option<BTreeMap<String, String>>,
}

/// 对应 `AssistantStreamObserver`：一次 assistant 流的进程本地生命周期观察者。
#[async_trait::async_trait]
pub trait AssistantStreamObserver: Send + Sync {
    async fn start(
        &self,
        message: AssistantMessage,
        event: AssistantMessageEvent,
        context: &Context,
    );
    async fn update(
        &self,
        message: AssistantMessage,
        event: AssistantMessageEvent,
        context: &Context,
    );
    async fn end(&self, message: SettledAssistantMessage, context: &Context);
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 对应 `transformContext` 的请求上下文。
#[derive(Clone)]
pub struct HarnessRequestContext {
    pub messages: Vec<AgentMessage>,
    pub system_prompt: String,
}

type TransformContextFn =
    Arc<dyn Fn(HarnessRequestContext, Context) -> BoxFuture<HarnessRequestContext> + Send + Sync>;
type ToProviderMessagesFn =
    Arc<dyn Fn(Vec<AgentMessage>, Context) -> BoxFuture<Vec<Message>> + Send + Sync>;
type BeforePayloadFn = Arc<
    dyn Fn(serde_json::Value, Model, Context) -> BoxFuture<Option<serde_json::Value>> + Send + Sync,
>;
type AfterResponseFn = Arc<
    dyn Fn(
            SettledAssistantMessage,
            AssistantResponseMetadata,
            Context,
        ) -> BoxFuture<SettledAssistantMessage>
        + Send
        + Sync,
>;
type AfterResponseSimpleFn = Arc<
    dyn Fn(SettledAssistantMessage, Context) -> BoxFuture<SettledAssistantMessage> + Send + Sync,
>;
type RequestFn = Arc<
    dyn Fn(pi_ai::Context, SimpleStreamOptions, Context) -> BoxFuture<AssistantMessageEventStream>
        + Send
        + Sync,
>;

/// 对应 `HarnessAssistantStreamConfig`。
pub struct HarnessAssistantStreamConfig {
    pub model: Model,
    pub system_prompt: String,
    pub tools: Option<Vec<Tool>>,
    pub thinking_level: ModelThinkingLevel,
    pub stream_options: AgentHarnessStreamOptions,
    pub transform_context: Option<TransformContextFn>,
    pub to_provider_messages: ToProviderMessagesFn,
    pub before_payload: Option<BeforePayloadFn>,
    pub after_response: Option<AfterResponseFn>,
    pub request: RequestFn,
    pub observer: Arc<dyn AssistantStreamObserver>,
}

/// 对应 `createRequestOptions` 的 `reasoning` 映射：`off` 之外才携带 reasoning。
fn thinking_level_to_reasoning(level: ModelThinkingLevel) -> Option<ThinkingLevel> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(ThinkingLevel::Max),
    }
}

/// 对应 `createRequestOptions`。
///
/// 注意：`telemetryContext`/`onPayload`/`onResponse` 在 pi-ai 的 provider 适配层处理，
/// 此处在 `AgentHarnessStreamOptions` 可表达的字段范围内构造选项。
fn create_request_options(
    config: &HarnessAssistantStreamConfig,
    context: &Context,
) -> SimpleStreamOptions {
    let options = &config.stream_options;
    SimpleStreamOptions {
        stream: StreamOptions {
            request: ProviderRequestOptions {
                signal: context.abort_signal().cloned(),
                headers: options
                    .headers
                    .clone()
                    .map(|h| h.into_iter().map(|(k, v)| (k, Some(v))).collect()),
                timeout_ms: options.timeout_ms,
                max_retries: options.max_retries,
                max_retry_delay_ms: options.max_retry_delay_ms,
                ..Default::default()
            },
            transport: options.transport,
            cache_retention: options.cache_retention,
            metadata: options.metadata.clone(),
            ..Default::default()
        },
        reasoning: thinking_level_to_reasoning(config.thinking_level),
        deferred: options.deferred.clone(),
        ..Default::default()
    }
}

/// 对应 `isUpdateEvent`：start/done/error 之外的均为 update 事件。
fn is_update_event(event: &AssistantMessageEvent) -> bool {
    !matches!(
        event,
        AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. }
    )
}

/// 提取任一事件携带的 partial assistant message。
fn event_partial(event: &AssistantMessageEvent) -> &AssistantMessage {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
        AssistantMessageEvent::Done { message, .. } => message,
        AssistantMessageEvent::Error { error, .. } => error,
    }
}

/// 对应 `consumeAssistantStream`。
pub async fn consume_assistant_stream(
    stream: AssistantMessageEventStream,
    observer: &Arc<dyn AssistantStreamObserver>,
    after_response: Option<AfterResponseSimpleFn>,
    context: &Context,
) -> SettledAssistantMessage {
    let mut started = false;
    while let Some(event) = stream.clone().next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                if started {
                    panic!("Assistant message stream emitted more than one start event");
                }
                started = true;
                observer
                    .start(partial.clone(), event.clone(), context)
                    .await;
            }
            AssistantMessageEvent::Done { .. } => {
                if !started {
                    panic!("Assistant message stream emitted done before start");
                }
            }
            AssistantMessageEvent::Error { .. } => {
                // 原版不显式处理 error 事件：result() 会返回 error message。
            }
            event if is_update_event(event) => {
                if !started {
                    panic!("Assistant message stream emitted an update event before start");
                }
                observer
                    .update(event_partial(event).clone(), event.clone(), context)
                    .await;
            }
            _ => {}
        }
    }

    let settled = stream.result().await;
    let mut final_message = settled;
    if let Some(after_response) = after_response {
        let result = after_response(final_message.clone(), context.clone()).await;
        final_message = result;
    }
    observer.end(final_message.clone(), context).await;
    final_message
}

/// 对应 `streamHarnessAssistant`：流式执行一次 assistant 响应，不修改调用方消息列表。
pub async fn stream_harness_assistant(
    messages: Vec<AgentMessage>,
    config: &HarnessAssistantStreamConfig,
    context: &Context,
) -> SettledAssistantMessage {
    let mut request_context = HarnessRequestContext {
        messages,
        system_prompt: config.system_prompt.clone(),
    };
    if let Some(transform) = &config.transform_context {
        request_context = transform(request_context, context.clone()).await;
    }

    let provider_messages =
        (config.to_provider_messages)(request_context.messages.clone(), context.clone()).await;
    let ai_context = pi_ai::Context {
        system_prompt: Some(request_context.system_prompt.clone()),
        messages: provider_messages,
        tools: config.tools.clone(),
    };

    let stream = (config.request)(
        ai_context,
        create_request_options(config, context),
        context.clone(),
    )
    .await;

    let after_response = config.after_response.clone().map(|after| {
        Arc::new(move |message: SettledAssistantMessage, _ctx: Context| {
            let after = after.clone();
            Box::pin(
                async move { after(message, AssistantResponseMetadata::default(), _ctx).await },
            ) as BoxFuture<SettledAssistantMessage>
        }) as AfterResponseSimpleFn
    });

    consume_assistant_stream(stream, &config.observer, after_response, context).await
}
