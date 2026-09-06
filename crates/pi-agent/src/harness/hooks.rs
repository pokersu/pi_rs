//! Rust 翻译自 packages/agent/src/harness/hooks.ts
//!
//! 有序 harness hook 注册表与聚合运行器。

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use serde_json::{Value as Json, json};

use crate::harness::context::{Context, with_abort_signal};
use crate::harness::execution::effect_gate::{Gate, GateError};
use crate::harness::types::{AgentHarnessStreamOptions, AgentHarnessStreamOptionsPatch};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 对应 `HookHandler`：事件与结果以 JSON 序列化传递（Rust 化折中）。
pub type HookHandlerFn = Arc<dyn Fn(Json, Context) -> BoxFuture<Json> + Send + Sync>;

/// 对应 `HookErrorReporter`。
pub type HookErrorReporter =
    Arc<dyn Fn(String, HookName, String, Context) -> BoxFuture<()> + Send + Sync>;

/// 对应 `HookName = keyof HookMap`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookName {
    BeforeRun,
    BeforeDrive,
    BeforeRunEnd,
    TransformContext,
    BeforeRequest,
    BeforePayload,
    AfterResponse,
    BeforeTool,
    AfterTool,
    BeforeCompaction,
    BeforeNavigation,
}

impl HookName {
    pub fn as_str(self) -> &'static str {
        match self {
            HookName::BeforeRun => "before_run",
            HookName::BeforeDrive => "before_drive",
            HookName::BeforeRunEnd => "before_run_end",
            HookName::TransformContext => "transform_context",
            HookName::BeforeRequest => "before_request",
            HookName::BeforePayload => "before_payload",
            HookName::AfterResponse => "after_response",
            HookName::BeforeTool => "before_tool",
            HookName::AfterTool => "after_tool",
            HookName::BeforeCompaction => "before_compaction",
            HookName::BeforeNavigation => "before_navigation",
        }
    }
}

struct HookRegistration {
    token: usize,
    id: Option<String>,
    handler: HookHandlerFn,
}

/// 对应 `on` 的 options。
#[derive(Default)]
pub struct HookOnOptions {
    pub id: Option<String>,
}

/// 对应 `HookRegistry`。
pub struct HookRegistry {
    registrations: Arc<Mutex<BTreeMap<HookName, Vec<HookRegistration>>>>,
    report_error: HookErrorReporter,
    closed_error: Arc<Mutex<Option<String>>>,
    next_token: Arc<AtomicUsize>,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Unknown hook error".to_string()
    }
}

/// 调用单个 handler，将 panic 归一化为 `Err(String)`。
async fn invoke_registration(
    handler: &HookHandlerFn,
    event: Json,
    context: &Context,
) -> Result<Json, String> {
    match AssertUnwindSafe(handler(event, context.clone()))
        .catch_unwind()
        .await
    {
        Ok(result) => Ok(result),
        Err(payload) => Err(panic_message(&payload)),
    }
}

fn merge_event(event: &Json, key: &str, value: Json) -> Json {
    let mut merged = event.clone();
    if let Some(obj) = merged.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
    merged
}

impl HookRegistry {
    pub fn new(report_error: HookErrorReporter) -> Self {
        Self {
            registrations: Arc::new(Mutex::new(BTreeMap::new())),
            report_error,
            closed_error: Arc::new(Mutex::new(None)),
            next_token: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 对应 `on`：注册一个 handler，返回退订闭包。
    pub fn on(
        &self,
        name: HookName,
        handler: HookHandlerFn,
        options: HookOnOptions,
    ) -> impl FnOnce() {
        if self.closed_error.lock().unwrap().is_some() {
            panic!("AgentHarness is closed");
        }
        let token = self.next_token.fetch_add(1, Ordering::SeqCst);
        let registration = HookRegistration {
            token,
            id: options.id,
            handler,
        };
        self.registrations
            .lock()
            .unwrap()
            .entry(name)
            .or_default()
            .push(registration);

        let registrations = Arc::clone(&self.registrations);
        move || {
            if let Some(list) = registrations.lock().unwrap().get_mut(&name) {
                list.retain(|registration| registration.token != token);
            }
        }
    }

    /// 对应 `has`。
    pub fn has(&self, name: HookName) -> bool {
        self.registrations
            .lock()
            .unwrap()
            .get(&name)
            .is_some_and(|list| !list.is_empty())
    }

    /// 对应 `runWithGate`：同步通过 effect gate 后执行一个 accepted-operation 聚合。
    pub async fn run_with_gate(
        &self,
        name: HookName,
        event: Json,
        gate: &Gate,
        context: &Context,
    ) -> Result<Json, GateError> {
        let gate_signal = gate.signal.clone();
        let context = context.clone();
        let admitted = gate.admit(move || {
            let admitted_context = with_abort_signal(&gate_signal, &context);
            if let Some(signal) = admitted_context.abort_signal() {
                signal.throw_if_aborted().expect("aborted");
            }
            async move { self.run_admitted(name, event, admitted_context).await }
        })?;
        admitted.await
    }

    /// 对应 `runToolWithGate`：执行一个工具 hook 聚合。
    pub async fn run_tool_with_gate(
        &self,
        name: HookName,
        event: Json,
        gate: &Gate,
        context: &Context,
    ) -> Result<Json, GateError> {
        debug_assert!(matches!(name, HookName::BeforeTool | HookName::AfterTool));
        self.run_with_gate(name, event, gate, context).await
    }

    /// 对应 `close`。
    pub fn close(&self, error: String) {
        let mut closed = self.closed_error.lock().unwrap();
        if closed.is_none() {
            *closed = Some(error);
        }
    }

    async fn run_admitted(
        &self,
        name: HookName,
        event: Json,
        context: Context,
    ) -> Result<Json, GateError> {
        if let Some(error) = self.closed_error.lock().unwrap().clone() {
            return Err(GateError::Closed(error));
        }
        Ok(self.aggregate(name, event, &context).await)
    }

    async fn aggregate(&self, name: HookName, event: Json, context: &Context) -> Json {
        match name {
            HookName::BeforeRun => self.before_run(event, context).await,
            HookName::BeforeDrive => {
                self.invoke_all_fail_closed(name, event, context).await;
                Json::Null
            }
            HookName::BeforeRunEnd => {
                let mut follow_up: Option<String> = None;
                self.invoke_all(
                    name,
                    &event,
                    |value: &Json| {
                        if let Some(result) = value.get("followUp").and_then(|v| v.as_str()) {
                            follow_up = Some(result.to_string());
                        }
                    },
                    context,
                )
                .await;
                match follow_up {
                    None => Json::Null,
                    Some(follow_up) => json!({ "followUp": follow_up }),
                }
            }
            HookName::TransformContext => self.transform_context(event, context).await,
            HookName::BeforeRequest => self.before_request(event, context).await,
            HookName::BeforePayload => self.before_payload(event, context).await,
            HookName::AfterResponse => self.after_response(event, context).await,
            HookName::BeforeTool => self.before_tool(event, context).await,
            HookName::AfterTool => self.after_tool(event, context).await,
            HookName::BeforeCompaction => {
                self.first_structural(name, event, "compaction", context)
                    .await
            }
            HookName::BeforeNavigation => {
                self.first_structural(name, event, "summary", context).await
            }
        }
    }

    async fn before_run(&self, event: Json, context: &Context) -> Json {
        let mut prompt = event.get("prompt").cloned().unwrap_or(Json::Array(vec![]));
        let mut injected: Vec<Json> = vec![];
        for registration in self.registrations_for(HookName::BeforeRun) {
            let result = invoke_registration(
                &registration.handler,
                merge_event(&event, "prompt", prompt.clone()),
                context,
            )
            .await;
            match result {
                Ok(result) => {
                    if let Some(messages) = result.get("messages").and_then(|v| v.as_array()) {
                        for message in messages {
                            injected.push(message.clone());
                        }
                        if let Some(prompt_array) = prompt.as_array_mut() {
                            prompt_array.extend(messages.iter().cloned());
                        }
                    }
                }
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::BeforeRun, &event, context)
                        .await;
                }
            }
        }
        if injected.is_empty() {
            Json::Null
        } else {
            json!({ "messages": injected })
        }
    }

    async fn before_tool(&self, event: Json, context: &Context) -> Json {
        let original_args = event.get("args").cloned().unwrap_or(Json::Null);
        let mut args = original_args.clone();
        let mut block: Option<Json> = None;
        for registration in self.registrations_for(HookName::BeforeTool) {
            let result = invoke_registration(
                &registration.handler,
                merge_event(&event, "args", args.clone()),
                context,
            )
            .await;
            match result {
                Ok(result) => {
                    if let Some(next_args) = result.get("args") {
                        args = next_args.clone();
                    }
                    if let Some(next_block) = result.get("block") {
                        block = Some(next_block.clone());
                        break;
                    }
                }
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::BeforeTool, &event, context)
                        .await;
                    block = Some(json!({ "reason": error }));
                    break;
                }
            }
        }
        let mut out = serde_json::Map::new();
        if args != original_args {
            out.insert("args".to_string(), args);
        }
        if let Some(block) = block {
            out.insert("block".to_string(), block);
        }
        Json::Object(out)
    }

    async fn transform_context(&self, event: Json, context: &Context) -> Json {
        let mut messages = event
            .get("messages")
            .cloned()
            .unwrap_or(Json::Array(vec![]));
        let mut system_prompt = event.get("systemPrompt").cloned().unwrap_or(Json::Null);
        for registration in self.registrations_for(HookName::TransformContext) {
            let mut updated = event.clone();
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("messages".to_string(), messages.clone());
                obj.insert("systemPrompt".to_string(), system_prompt.clone());
            }
            let result = invoke_registration(&registration.handler, updated, context).await;
            match result {
                Ok(result) => {
                    if let Some(next) = result.get("messages") {
                        messages = next.clone();
                    }
                    if let Some(next) = result.get("systemPrompt") {
                        system_prompt = next.clone();
                    }
                }
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::TransformContext, &event, context)
                        .await;
                }
            }
        }
        json!({ "messages": messages, "systemPrompt": system_prompt })
    }

    async fn before_request(&self, event: Json, context: &Context) -> Json {
        let base: AgentHarnessStreamOptions =
            serde_json::from_value(event.get("streamOptions").cloned().unwrap_or(Json::Null))
                .unwrap_or_default();
        let mut stream_options = base.clone();
        let mut changed = false;
        for registration in self.registrations_for(HookName::BeforeRequest) {
            let updated = merge_event(
                &event,
                "streamOptions",
                serde_json::to_value(&stream_options).unwrap_or(Json::Null),
            );
            let result = invoke_registration(&registration.handler, updated, context).await;
            match result {
                Ok(result) => {
                    if let Some(patch) = result.get("streamOptions")
                        && let Ok(patch) =
                            serde_json::from_value::<AgentHarnessStreamOptionsPatch>(patch.clone())
                    {
                        stream_options = apply_stream_options_patch(&stream_options, &patch);
                        changed = true;
                    }
                }
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::BeforeRequest, &event, context)
                        .await;
                }
            }
        }
        if changed {
            let patch = create_stream_options_patch(&base, &stream_options);
            json!({ "streamOptions": patch })
        } else {
            Json::Null
        }
    }

    async fn before_payload(&self, event: Json, context: &Context) -> Json {
        let mut payload = event.get("payload").cloned().unwrap_or(Json::Null);
        for registration in self.registrations_for(HookName::BeforePayload) {
            let result = invoke_registration(
                &registration.handler,
                merge_event(&event, "payload", payload.clone()),
                context,
            )
            .await;
            match result {
                Ok(result) => {
                    if let Some(next) = result.get("payload") {
                        payload = next.clone();
                    }
                }
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::BeforePayload, &event, context)
                        .await;
                }
            }
        }
        json!({ "payload": payload })
    }

    async fn after_response(&self, event: Json, context: &Context) -> Json {
        let mut message = event.get("message").cloned().unwrap_or(Json::Null);
        for registration in self.registrations_for(HookName::AfterResponse) {
            let result = invoke_registration(
                &registration.handler,
                merge_event(&event, "message", message.clone()),
                context,
            )
            .await;
            match result {
                Ok(result) => {
                    if let Some(next) = result.get("message") {
                        message = next.clone();
                    }
                }
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::AfterResponse, &event, context)
                        .await;
                }
            }
        }
        json!({ "message": message })
    }

    async fn after_tool(&self, event: Json, context: &Context) -> Json {
        let mut current = json!({
            "content": event.get("content").cloned().unwrap_or(Json::Null),
            "details": event.get("details").cloned().unwrap_or(Json::Null),
            "isError": event.get("isError").cloned().unwrap_or(Json::Bool(false)),
            "usage": event.get("usage").cloned().unwrap_or(Json::Null),
        });
        let mut aggregate = serde_json::Map::new();
        for registration in self.registrations_for(HookName::AfterTool) {
            let mut updated = event.clone();
            if let (Some(obj), Some(cur)) = (updated.as_object_mut(), current.as_object()) {
                for (key, value) in cur {
                    obj.insert(key.clone(), value.clone());
                }
            }
            let result = invoke_registration(&registration.handler, updated, context).await;
            match result {
                Ok(result) if !result.is_null() => {
                    if let Some(v) = result.get("content") {
                        aggregate.insert("content".to_string(), v.clone());
                    }
                    if let Some(v) = result.get("details") {
                        aggregate.insert("details".to_string(), v.clone());
                    }
                    if let Some(v) = result.get("isError") {
                        aggregate.insert("isError".to_string(), v.clone());
                    }
                    if let Some(v) = result.get("usage") {
                        aggregate.insert("usage".to_string(), v.clone());
                    }
                    if let Some(v) = result.get("terminate") {
                        aggregate.insert("terminate".to_string(), v.clone());
                    }
                    current = json!({
                        "content": result.get("content").unwrap_or(&current["content"]).clone(),
                        "details": result.get("details").unwrap_or(&current["details"]).clone(),
                        "isError": result.get("isError").unwrap_or(&current["isError"]).clone(),
                        "usage": result.get("usage").unwrap_or(&current["usage"]).clone(),
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = self
                        .report_hook_error(&error, HookName::AfterTool, &event, context)
                        .await;
                }
            }
        }
        if aggregate.is_empty() {
            Json::Null
        } else {
            Json::Object(aggregate)
        }
    }

    async fn first_structural(
        &self,
        name: HookName,
        event: Json,
        result_field: &str,
        context: &Context,
    ) -> Json {
        for registration in self.registrations_for(name) {
            let result = invoke_registration(&registration.handler, event.clone(), context).await;
            match result {
                Ok(value) => {
                    if !value.is_object() {
                        continue;
                    }
                    let obj = value.as_object().unwrap();
                    let declined = obj
                        .get("decline")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if declined && obj.get(result_field).is_some() {
                        let _ = self
                            .report_hook_error(
                                &format!(
                                    "{name:?} hook cannot return both decline and {result_field}"
                                ),
                                name,
                                &event,
                                context,
                            )
                            .await;
                        continue;
                    }
                    if declined || obj.get(result_field).is_some() {
                        return value;
                    }
                }
                Err(error) => {
                    let _ = self.report_hook_error(&error, name, &event, context).await;
                }
            }
        }
        Json::Null
    }

    fn registrations_for(&self, name: HookName) -> Vec<HookRegistration> {
        self.registrations
            .lock()
            .unwrap()
            .get(&name)
            .map(|list| {
                list.iter()
                    .map(|r| HookRegistration {
                        token: r.token,
                        id: r.id.clone(),
                        handler: Arc::clone(&r.handler),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn report_hook_error(
        &self,
        error: &str,
        name: HookName,
        event: &Json,
        context: &Context,
    ) {
        let lane = event
            .get("lane")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let _ = (self.report_error)(error.to_string(), name, lane, context.clone()).await;
    }

    async fn invoke_all_fail_closed(&self, name: HookName, event: Json, context: &Context) {
        for registration in self.registrations_for(name) {
            let result = invoke_registration(&registration.handler, event.clone(), context).await;
            if let Err(error) = result {
                let lane = event
                    .get("lane")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ =
                    (self.report_error)(error.clone(), name, lane.clone(), context.clone()).await;
                panic!("{error}");
            }
        }
    }

    async fn invoke_all(
        &self,
        name: HookName,
        event: &Json,
        mut apply: impl FnMut(&Json),
        context: &Context,
    ) {
        for registration in self.registrations_for(name) {
            let result = invoke_registration(&registration.handler, event.clone(), context).await;
            match result {
                Ok(value) => apply(&value),
                Err(error) => {
                    let _ = self.report_hook_error(&error, name, event, context).await;
                }
            }
        }
    }
}

/// 对应 `applyStreamOptionsPatch`。
pub fn apply_stream_options_patch(
    base: &AgentHarnessStreamOptions,
    patch: &AgentHarnessStreamOptionsPatch,
) -> AgentHarnessStreamOptions {
    let mut next = base.clone();
    if let Some(v) = patch.transport {
        next.transport = Some(v);
    }
    if let Some(v) = patch.timeout_ms {
        next.timeout_ms = Some(v);
    }
    if let Some(v) = patch.max_retries {
        next.max_retries = Some(v);
    }
    if let Some(v) = patch.max_retry_delay_ms {
        next.max_retry_delay_ms = Some(v);
    }
    if let Some(v) = patch.cache_retention {
        next.cache_retention = Some(v);
    }
    if let Some(v) = patch.deferred.clone() {
        next.deferred = Some(v);
    }
    if let Some(headers_patch) = &patch.headers {
        let mut headers = next.headers.clone().unwrap_or_default();
        for (key, value) in headers_patch {
            match value {
                Some(v) => {
                    headers.insert(key.clone(), v.clone());
                }
                None => {
                    headers.remove(key);
                }
            }
        }
        next.headers = Some(headers);
    }
    if let Some(metadata_patch) = &patch.metadata {
        let mut metadata = next
            .metadata
            .clone()
            .and_then(|m| m.as_object().cloned())
            .unwrap_or_default();
        for (key, value) in metadata_patch {
            match value {
                Some(v) => {
                    metadata.insert(key.clone(), v.clone());
                }
                None => {
                    metadata.remove(key);
                }
            }
        }
        next.metadata = Some(Json::Object(metadata));
    }
    next
}

/// 对应 `createStreamOptionsPatch`。
pub fn create_stream_options_patch(
    base: &AgentHarnessStreamOptions,
    value: &AgentHarnessStreamOptions,
) -> AgentHarnessStreamOptionsPatch {
    let mut patch = AgentHarnessStreamOptionsPatch::default();
    if base.transport != value.transport {
        patch.transport = value.transport;
    }
    if base.timeout_ms != value.timeout_ms {
        patch.timeout_ms = value.timeout_ms;
    }
    if base.max_retries != value.max_retries {
        patch.max_retries = value.max_retries;
    }
    if base.max_retry_delay_ms != value.max_retry_delay_ms {
        patch.max_retry_delay_ms = value.max_retry_delay_ms;
    }
    if base.cache_retention != value.cache_retention {
        patch.cache_retention = value.cache_retention;
    }
    if base.deferred != value.deferred {
        patch.deferred = value.deferred.clone();
    }
    if base.headers != value.headers {
        let mut headers: BTreeMap<String, Option<String>> = BTreeMap::new();
        for key in base.headers.clone().unwrap_or_default().keys() {
            if !value.headers.as_ref().is_some_and(|h| h.contains_key(key)) {
                headers.insert(key.clone(), None);
            }
        }
        for (key, header) in value.headers.clone().unwrap_or_default() {
            if base.headers.as_ref().and_then(|h| h.get(&key)) != Some(&header) {
                headers.insert(key, Some(header));
            }
        }
        patch.headers = Some(headers);
    }
    if base.metadata != value.metadata {
        let mut metadata: BTreeMap<String, Option<Json>> = BTreeMap::new();
        let base_obj = base
            .metadata
            .as_ref()
            .and_then(|m| m.as_object())
            .cloned()
            .unwrap_or_default();
        let value_obj = value
            .metadata
            .as_ref()
            .and_then(|m| m.as_object())
            .cloned()
            .unwrap_or_default();
        for key in base_obj.keys() {
            if !value_obj.contains_key(key) {
                metadata.insert(key.clone(), None);
            }
        }
        for (key, header) in value_obj {
            if base_obj.get(&key) != Some(&header) {
                metadata.insert(key, Some(header));
            }
        }
        patch.metadata = Some(metadata);
    }
    patch
}
