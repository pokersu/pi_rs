//! Rust 翻译自 packages/agent/src/agent.ts
//!
//! 底层 agent 循环的有状态封装：持有 transcript、发射生命周期事件、执行工具，
//! 并暴露 steering/follow-up 消息队列。

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use pi_ai::{
    AbortSignal, AssistantMessage, Message, Model, StopReason, TextContent, TextKind,
    ThinkingBudgets, Transport, default_usage,
};

use crate::agent_loop::{AgentEventSink, run_agent_loop, run_agent_loop_continue};
use crate::stream_fn::get_default_stream_fn;
use crate::types::{
    AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentState,
    AgentTool, BeforeToolCallFn, ConvertToLlmFn, GetApiKeyFn, GetMessagesFn, PrepareNextTurnFn,
    QueueMode, ShouldStopAfterTurnFn, StreamFn, ThinkingLevel, ToolExecutionMode,
    TransformContextFn,
};

/// 对应 `defaultConvertToLlm`
fn default_convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    crate::harness::messages::convert_to_llm(messages)
}

fn default_model() -> Model {
    Model {
        id: "unknown".to_string(),
        name: "unknown".to_string(),
        api: "unknown".to_string(),
        provider: "unknown".to_string(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: pi_ai::ModelCost {
            rates: pi_ai::ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: None,
        },
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// 对应 `PendingMessageQueue`
struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        if self.mode == QueueMode::All {
            return std::mem::take(&mut self.messages);
        }
        if self.messages.is_empty() {
            return Vec::new();
        }
        vec![self.messages.remove(0)]
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

/// 对应 `ActiveRun`
struct ActiveRun {
    signal: AbortSignal,
    done: Arc<tokio::sync::Notify>,
}

type AgentListener =
    Arc<dyn Fn(AgentEvent, AbortSignal) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 对应 `MutableAgentState`
struct MutableAgentState {
    system_prompt: String,
    model: Model,
    thinking_level: ThinkingLevel,
    tools: Vec<AgentTool>,
    messages: Vec<AgentMessage>,
    is_streaming: bool,
    streaming_message: Option<AgentMessage>,
    pending_tool_calls: BTreeSet<String>,
    error_message: Option<String>,
}

impl MutableAgentState {
    fn snapshot(&self) -> AgentState {
        AgentState {
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            tools: self.tools.clone(),
            messages: self.messages.clone(),
            is_streaming: self.is_streaming,
            streaming_message: self.streaming_message.clone(),
            pending_tool_calls: self.pending_tool_calls.clone(),
            error_message: self.error_message.clone(),
        }
    }
}

struct AgentInner {
    state: MutableAgentState,
    listeners: Vec<AgentListener>,
    steering_queue: PendingMessageQueue,
    follow_up_queue: PendingMessageQueue,
    active_run: Option<ActiveRun>,
    convert_to_llm: ConvertToLlmFn,
    transform_context: Option<TransformContextFn>,
    stream_function: StreamFn,
    get_api_key: Option<GetApiKeyFn>,
    before_tool_call: Option<BeforeToolCallFn>,
    after_tool_call: Option<AfterToolCallFn>,
    should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    prepare_next_turn: Option<PrepareNextTurnFn>,
    prepare_next_turn_with_context: Option<PrepareNextTurnFn>,
    session_id: Option<String>,
    thinking_budgets: Option<ThinkingBudgets>,
    transport: Transport,
    max_retry_delay_ms: Option<u64>,
    tool_execution: ToolExecutionMode,
}

impl AgentInner {
    fn create_context_snapshot(&self) -> AgentContext {
        AgentContext {
            system_prompt: self.state.system_prompt.clone(),
            messages: self.state.messages.clone(),
            tools: Some(self.state.tools.clone()),
        }
    }

    fn create_loop_config(&self, arc: &Arc<Mutex<AgentInner>>) -> AgentLoopConfig {
        let should_stop = self.should_stop_after_turn.clone();
        let prepare = self
            .prepare_next_turn_with_context
            .clone()
            .or_else(|| self.prepare_next_turn.clone());
        AgentLoopConfig {
            model: self.state.model.clone(),
            stream: pi_ai::SimpleStreamOptions {
                stream: pi_ai::StreamOptions {
                    request: pi_ai::ProviderRequestOptions {
                        max_retry_delay_ms: self.max_retry_delay_ms,
                        ..Default::default()
                    },
                    transport: Some(self.transport),
                    session_id: self.session_id.clone(),
                    ..Default::default()
                },
                reasoning: crate::types::to_ai_thinking_level(self.state.thinking_level),
                thinking_budgets: self.thinking_budgets,
                ..Default::default()
            },
            convert_to_llm: self.convert_to_llm.clone(),
            transform_context: self.transform_context.clone(),
            get_api_key: self.get_api_key.clone(),
            should_stop_after_turn: should_stop,
            prepare_next_turn: prepare,
            get_steering_messages: Some(make_queue_drain(arc.clone(), true)),
            get_follow_up_messages: Some(make_queue_drain(arc.clone(), false)),
            before_tool_call: self.before_tool_call.clone(),
            after_tool_call: self.after_tool_call.clone(),
            tool_execution: self.tool_execution,
        }
    }
}

/// 对应 `AgentOptions`
#[derive(Default)]
pub struct AgentOptions {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Option<Vec<AgentTool>>,
    pub messages: Option<Vec<AgentMessage>>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub prepare_next_turn_with_context: Option<PrepareNextTurnFn>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub transport: Option<Transport>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: Option<ToolExecutionMode>,
}

/// 对应 `Agent` 类。
pub struct Agent {
    inner: Arc<Mutex<AgentInner>>,
}

impl Agent {
    pub fn new(options: AgentOptions) -> Self {
        let inner = AgentInner {
            state: MutableAgentState {
                system_prompt: options.system_prompt.unwrap_or_default(),
                model: options.model.unwrap_or_else(default_model),
                thinking_level: options.thinking_level.unwrap_or(ThinkingLevel::Off),
                tools: options.tools.unwrap_or_default(),
                messages: options.messages.unwrap_or_default(),
                is_streaming: false,
                streaming_message: None,
                pending_tool_calls: BTreeSet::new(),
                error_message: None,
            },
            listeners: Vec::new(),
            steering_queue: PendingMessageQueue::new(
                options.steering_mode.unwrap_or(QueueMode::OneAtATime),
            ),
            follow_up_queue: PendingMessageQueue::new(
                options.follow_up_mode.unwrap_or(QueueMode::OneAtATime),
            ),
            active_run: None,
            convert_to_llm: options
                .convert_to_llm
                .unwrap_or_else(|| Arc::new(default_convert_to_llm)),
            transform_context: options.transform_context,
            stream_function: options.stream_fn.unwrap_or_else(get_default_stream_fn),
            get_api_key: options.get_api_key,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            should_stop_after_turn: options.should_stop_after_turn,
            prepare_next_turn: options.prepare_next_turn,
            prepare_next_turn_with_context: options.prepare_next_turn_with_context,
            session_id: options.session_id,
            thinking_budgets: options.thinking_budgets,
            transport: options.transport.unwrap_or(Transport::Auto),
            max_retry_delay_ms: options.max_retry_delay_ms,
            tool_execution: options
                .tool_execution
                .unwrap_or(ToolExecutionMode::Parallel),
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// 对应 `subscribe`
    pub fn subscribe<F>(&self, listener: F)
    where
        F: Fn(AgentEvent, AbortSignal) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.inner
            .lock()
            .unwrap()
            .listeners
            .push(Arc::new(listener));
    }

    /// 对应 `state` getter
    pub fn state(&self) -> AgentState {
        self.inner.lock().unwrap().state.snapshot()
    }

    /// 对应 `state.tools = ...`
    pub fn set_tools(&self, tools: Vec<AgentTool>) {
        self.inner.lock().unwrap().state.tools = tools;
    }

    /// 对应 `state.messages = ...`
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.inner.lock().unwrap().state.messages = messages;
    }

    /// 对应 `state.systemPrompt = ...`
    pub fn set_system_prompt(&self, prompt: String) {
        self.inner.lock().unwrap().state.system_prompt = prompt;
    }

    /// 对应 `state.model = ...`
    pub fn set_model(&self, model: Model) {
        self.inner.lock().unwrap().state.model = model;
    }

    /// 对应 `state.thinkingLevel = ...`
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        self.inner.lock().unwrap().state.thinking_level = level;
    }

    /// 对应 `steer(message)`
    pub fn steer(&self, message: AgentMessage) {
        self.inner.lock().unwrap().steering_queue.enqueue(message);
    }

    /// 对应 `followUp(message)`
    pub fn follow_up(&self, message: AgentMessage) {
        self.inner.lock().unwrap().follow_up_queue.enqueue(message);
    }

    /// 对应 `clearSteeringQueue`
    pub fn clear_steering_queue(&self) {
        self.inner.lock().unwrap().steering_queue.clear();
    }

    /// 对应 `clearFollowUpQueue`
    pub fn clear_follow_up_queue(&self) {
        self.inner.lock().unwrap().follow_up_queue.clear();
    }

    /// 对应 `clearAllQueues`
    pub fn clear_all_queues(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.steering_queue.clear();
        inner.follow_up_queue.clear();
    }

    /// 对应 `hasQueuedMessages`
    pub fn has_queued_messages(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.steering_queue.has_items() || inner.follow_up_queue.has_items()
    }

    /// 对应 `signal` getter
    pub fn signal(&self) -> Option<AbortSignal> {
        self.inner
            .lock()
            .unwrap()
            .active_run
            .as_ref()
            .map(|r| r.signal.clone())
    }

    /// 对应 `abort()`
    pub fn abort(&self) {
        if let Some(run) = &self.inner.lock().unwrap().active_run {
            run.signal.abort();
        }
    }

    /// 对应 `waitForIdle()`
    pub async fn wait_for_idle(&self) {
        let done = self
            .inner
            .lock()
            .unwrap()
            .active_run
            .as_ref()
            .map(|r| r.done.clone());
        if let Some(done) = done {
            done.notified().await;
        }
    }

    /// 对应 `reset()`
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.active_run.is_some() {
            panic!("Agent is already processing. Wait for completion before resetting.");
        }
        inner.state.messages.clear();
        inner.state.is_streaming = false;
        inner.state.streaming_message = None;
        inner.state.pending_tool_calls.clear();
        inner.state.error_message = None;
        inner.follow_up_queue.clear();
        inner.steering_queue.clear();
    }

    /// 对应 `prompt`（文本输入）。
    pub async fn prompt_text(&self, text: &str) {
        let message = AgentMessage::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text(text.to_string()),
            timestamp: pi_ai::utils::uuid::now_ms() as u64,
        });
        self.prompt_messages(vec![message]).await;
    }

    /// 对应 `prompt`（消息输入）。
    pub async fn prompt_messages(&self, messages: Vec<AgentMessage>) {
        if self.inner.lock().unwrap().active_run.is_some() {
            panic!(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
            );
        }
        self.run_prompt_messages(messages).await;
    }

    /// 对应 `continue()`
    pub async fn continue_turn(&self) {
        let (steering, follow_ups, last_role) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.active_run.is_some() {
                panic!("Agent is already processing. Wait for completion before continuing.");
            }
            let last_role = inner.state.messages.last().map(|m| m.role());
            let steering = inner.steering_queue.drain();
            let follow_ups = inner.follow_up_queue.drain();
            (steering, follow_ups, last_role)
        };

        match last_role {
            None => panic!("No messages to continue from"),
            Some("assistant") => {
                if !steering.is_empty() {
                    self.run_prompt_messages(steering).await;
                    return;
                }
                if !follow_ups.is_empty() {
                    self.run_prompt_messages(follow_ups).await;
                    return;
                }
                panic!("Cannot continue from message role: assistant");
            }
            _ => {}
        }
        self.run_continuation().await;
    }

    // --- 内部 ---

    async fn run_prompt_messages(&self, messages: Vec<AgentMessage>) {
        let inner = self.inner.clone();
        self.run_with_lifecycle(move |signal| {
            let inner = inner.clone();
            Box::pin(async move {
                let (context, config, stream_fn, emit) = {
                    let guard = inner.lock().unwrap();
                    (
                        guard.create_context_snapshot(),
                        guard.create_loop_config(&inner),
                        guard.stream_function.clone(),
                        make_emit(&inner),
                    )
                };
                run_agent_loop(messages, context, config, Some(signal), emit, stream_fn).await;
            })
        })
        .await;
    }

    async fn run_continuation(&self) {
        let inner = self.inner.clone();
        self.run_with_lifecycle(move |signal| {
            let inner = inner.clone();
            Box::pin(async move {
                let (context, config, stream_fn, emit) = {
                    let guard = inner.lock().unwrap();
                    (
                        guard.create_context_snapshot(),
                        guard.create_loop_config(&inner),
                        guard.stream_function.clone(),
                        make_emit(&inner),
                    )
                };
                run_agent_loop_continue(context, config, Some(signal), emit, stream_fn).await;
            })
        })
        .await;
    }

    /// 对应 `runWithLifecycle`
    async fn run_with_lifecycle<F>(&self, executor: F)
    where
        F: FnOnce(AbortSignal) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
    {
        let signal = AbortSignal::new();
        let done = Arc::new(tokio::sync::Notify::new());
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.active_run.is_some() {
                panic!("Agent is already processing.");
            }
            inner.active_run = Some(ActiveRun {
                signal: signal.clone(),
                done: done.clone(),
            });
            inner.state.is_streaming = true;
            inner.state.streaming_message = None;
            inner.state.error_message = None;
        }

        let result = std::panic::AssertUnwindSafe(executor(signal.clone()))
            .catch_unwind()
            .await;
        if let Err(payload) = result {
            self.handle_run_failure(signal.aborted(), &payload).await;
        }
        self.finish_run();
        done.notify_waiters();
    }

    async fn handle_run_failure(&self, aborted: bool, payload: &(dyn std::any::Any + Send)) {
        let message = failure_message(aborted, panic_text(payload));
        let events = [
            AgentEvent::MessageStart {
                message: message.clone(),
            },
            AgentEvent::MessageEnd {
                message: message.clone(),
            },
            AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: Vec::new(),
            },
            AgentEvent::AgentEnd {
                messages: vec![message],
            },
        ];
        let emit = make_emit(&self.inner);
        for event in events {
            emit(event).await;
        }
    }

    fn finish_run(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.is_streaming = false;
        inner.state.streaming_message = None;
        inner.state.pending_tool_calls.clear();
        inner.active_run = None;
    }
}

fn make_emit(inner: &Arc<Mutex<AgentInner>>) -> AgentEventSink {
    let inner = inner.clone();
    Arc::new(move |event| {
        let inner = inner.clone();
        Box::pin(async move { process_events(&inner, event).await })
    })
}

fn make_queue_drain(inner: Arc<Mutex<AgentInner>>, steering: bool) -> GetMessagesFn {
    Arc::new(move || {
        let inner = inner.clone();
        Box::pin(async move {
            let mut guard = inner.lock().unwrap();
            if steering {
                guard.steering_queue.drain()
            } else {
                guard.follow_up_queue.drain()
            }
        })
    })
}

/// 对应 `processEvents`
async fn process_events(inner: &Arc<Mutex<AgentInner>>, event: AgentEvent) {
    let signal = {
        let mut guard = inner.lock().unwrap();
        match &event {
            AgentEvent::MessageStart { message } => {
                guard.state.streaming_message = Some(message.clone());
            }
            AgentEvent::MessageUpdate { message, .. } => {
                guard.state.streaming_message = Some(message.clone());
            }
            AgentEvent::MessageEnd { message } => {
                guard.state.streaming_message = None;
                guard.state.messages.push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                guard.state.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                guard.state.pending_tool_calls.remove(tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Assistant(a) = message
                    && a.error_message.is_some()
                {
                    guard.state.error_message = a.error_message.clone();
                }
            }
            AgentEvent::AgentEnd { .. } => {
                guard.state.streaming_message = None;
            }
            _ => {}
        }
        guard.active_run.as_ref().map(|r| r.signal.clone())
    };

    let listeners = inner.lock().unwrap().listeners.clone();
    let signal = signal.expect("Agent listener invoked outside active run");
    for listener in listeners {
        listener(event.clone(), signal.clone()).await;
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    "agent run failed".to_string()
}

fn failure_message(aborted: bool, message: String) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content: vec![pi_ai::ContentBlock::Text(TextContent {
            kind: TextKind,
            text: String::new(),
            text_signature: None,
        })],
        api: "unknown".to_string(),
        provider: "unknown".to_string(),
        model: "unknown".to_string(),
        response_model: None,
        response_id: None,
        usage: default_usage(),
        stop_reason: if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        },
        deferred: None,
        error_message: Some(message),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    })
}
