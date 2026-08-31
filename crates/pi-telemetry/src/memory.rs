//! Rust 翻译自 packages/telemetry/src/memory.rs
//!
//! 后端中立的参考实现，把 span 记录在进程内存中。

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::FutureExt;

use crate::{
    AttributeValue, ErasedSpanCallback, ErasedSpanFuture, NOOP_TELEMETRY_CONTEXT, SpanAttributes,
    SpanError, SpanOptions, SpanStatus, TelemetryContext, TelemetrySpan,
};

/// 对应 `RecordedTelemetryEvent`
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

/// 对应 `RecordedTelemetrySpan`
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    pub end_sequence: Option<u64>,
}

/// 对应 `MutableRecordedTelemetryEvent`
struct MutableRecordedTelemetryEvent {
    name: String,
    attributes: SpanAttributes,
}

/// 对应 `MutableRecordedTelemetrySpan`
struct MutableRecordedTelemetrySpan {
    id: u64,
    parent_id: Option<u64>,
    name: String,
    attributes: SpanAttributes,
    events: Vec<MutableRecordedTelemetryEvent>,
    status: SpanStatus,
    explicit_status: bool,
    settled: bool,
    end_sequence: Option<u64>,
}

/// 对应 `InMemoryTelemetryState`
struct InMemoryTelemetryState {
    spans: Vec<MutableRecordedTelemetrySpan>,
    next_span_id: u64,
    next_end_sequence: u64,
}

impl InMemoryTelemetryState {
    fn new() -> Self {
        // 对应 TS 的 { spans: [], nextSpanId: 1, nextEndSequence: 1 }
        Self {
            spans: Vec::new(),
            next_span_id: 1,
            next_end_sequence: 1,
        }
    }
}

/// 对应 `copyAttributeValue`
fn copy_attribute_value(value: &AttributeValue) -> AttributeValue {
    match value {
        AttributeValue::StringArray(v) => AttributeValue::StringArray(v.clone()),
        AttributeValue::NumberArray(v) => AttributeValue::NumberArray(v.clone()),
        AttributeValue::BooleanArray(v) => AttributeValue::BooleanArray(v.clone()),
        other => other.clone(),
    }
}

/// 对应 `copyAttributes`
fn copy_attributes(attributes: Option<&SpanAttributes>) -> SpanAttributes {
    let mut copy = SpanAttributes::new();
    let Some(attributes) = attributes else {
        return copy;
    };
    for (name, value) in attributes {
        if let Some(v) = value {
            copy.insert(name.clone(), Some(copy_attribute_value(v)));
        }
    }
    copy
}

/// 对应 `mergeAttributes`
fn merge_attributes(current: &SpanAttributes, attributes: &SpanAttributes) -> SpanAttributes {
    let mut merged = copy_attributes(Some(current));
    for (name, value) in attributes {
        if let Some(v) = value {
            merged.insert(name.clone(), Some(copy_attribute_value(v)));
        }
    }
    merged
}

/// 对应 `copyStatus`
fn copy_status(status: &SpanStatus) -> SpanStatus {
    match status {
        SpanStatus::Ok => SpanStatus::Ok,
        SpanStatus::Error { error: Some(e) } => SpanStatus::Error {
            error: Some(SpanError {
                name: e.name.clone(),
                message: e.message.clone(),
            }),
        },
        SpanStatus::Error { error: None } => SpanStatus::Error { error: None },
    }
}

/// 从 panic payload 提取 message 文本（对应 TS 中 `Error` 的 name/message 检查）。
fn extract_panic_message(error: &(dyn Any + Send)) -> Option<String> {
    if let Some(s) = error.downcast_ref::<String>() {
        return Some(s.clone());
    }
    if let Some(s) = error.downcast_ref::<&str>() {
        return Some((*s).to_string());
    }
    None
}

/// 对应 `automaticErrorStatus`
fn automatic_error_status(error: &(dyn Any + Send)) -> SpanStatus {
    match extract_panic_message(error) {
        Some(message) => SpanStatus::Error {
            error: Some(SpanError {
                name: "Error".to_string(),
                message,
            }),
        },
        None => SpanStatus::Error { error: None },
    }
}

/// 对应 `settleSpan`
fn settle_span(
    state: &Mutex<InMemoryTelemetryState>,
    index: usize,
    failed: bool,
    error: Option<&(dyn Any + Send)>,
) {
    let mut st = state.lock().unwrap();
    if st.spans[index].settled {
        return;
    }
    let next_end = st.next_end_sequence;
    st.next_end_sequence += 1;
    let span = &mut st.spans[index];
    if failed && !span.explicit_status {
        span.status = match error {
            Some(e) => automatic_error_status(e),
            None => SpanStatus::Error { error: None },
        };
    }
    span.settled = true;
    span.end_sequence = Some(next_end);
}

/// 对应 `createSpan`。返回新 span 在 `state.spans` 中的索引。
fn create_span(
    state: &Mutex<InMemoryTelemetryState>,
    parent: Option<usize>,
    options: &SpanOptions,
) -> usize {
    let mut st = state.lock().unwrap();
    let id = st.next_span_id;
    st.next_span_id += 1;
    let parent_id = parent.map(|p| st.spans[p].id);
    let span = MutableRecordedTelemetrySpan {
        id,
        parent_id,
        name: options.name.clone(),
        attributes: copy_attributes(options.attributes.as_ref()),
        events: Vec::new(),
        status: SpanStatus::Ok,
        explicit_status: false,
        settled: false,
        end_sequence: None,
    };
    st.spans.push(span);
    st.spans.len() - 1
}

/// 对应 `InMemoryTelemetryState` 中记录的一个 span 的运行时句柄。
struct InMemoryTelemetrySpan {
    state: Arc<Mutex<InMemoryTelemetryState>>,
    index: usize,
}

/// 对应 `startInMemorySpan`（核心递归实现，`T` 为 callback 的返回类型）。
async fn start_in_memory_span<F, Fut, T>(
    state: Arc<Mutex<InMemoryTelemetryState>>,
    parent: Option<usize>,
    options: SpanOptions,
    callback: F,
) -> T
where
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send,
    Fut: Future<Output = T> + Send,
    T: Send,
{
    // 对应 TS: if (parent?.settled) return NOOP_TELEMETRY_CONTEXT.startSpan(...)
    if let Some(p) = parent
        && state.lock().unwrap().spans[p].settled
    {
        return NOOP_TELEMETRY_CONTEXT.start_span(options, callback).await;
    }

    // 对应 TS: try { createSpan; push } catch { return NOOP }
    let index = create_span(&state, parent, &options);

    let span: Arc<dyn TelemetrySpan> = Arc::new(InMemoryTelemetrySpan {
        state: state.clone(),
        index,
    });

    // 对应 TS: try { result = callback(span) } catch { settle(true, error); reject }
    //           Promise.resolve(result).then(ok -> settle(false), err -> settle(true, err) + throw)
    let fut = catch_unwind(AssertUnwindSafe(|| callback(span)));
    let result: Result<T, Box<dyn Any + Send>> = match fut {
        Ok(fut) => AssertUnwindSafe(fut).catch_unwind().await,
        Err(payload) => Err(payload),
    };

    match result {
        Ok(value) => {
            settle_span(&state, index, false, None);
            value
        }
        Err(payload) => {
            settle_span(&state, index, true, Some(payload.as_ref()));
            resume_unwind(payload)
        }
    }
}

impl TelemetryContext for InMemoryTelemetrySpan {
    fn start_span<'a, F, Fut, T>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
        T: Send + 'a,
    {
        let state = self.state.clone();
        let index = self.index;
        Box::pin(async move { start_in_memory_span(state, Some(index), options, callback).await })
    }
}

impl TelemetrySpan for InMemoryTelemetrySpan {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>) {
        let mut st = self.state.lock().unwrap();
        let span = &mut st.spans[self.index];
        if span.settled {
            return;
        }
        span.events.push(MutableRecordedTelemetryEvent {
            name: name.to_string(),
            attributes: copy_attributes(attributes.as_ref()),
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        let mut st = self.state.lock().unwrap();
        let span = &mut st.spans[self.index];
        if span.settled {
            return;
        }
        let merged = merge_attributes(&span.attributes, &attributes);
        span.attributes = merged;
    }

    fn set_status(&self, status: SpanStatus) {
        let mut st = self.state.lock().unwrap();
        let span = &mut st.spans[self.index];
        if span.settled {
            return;
        }
        span.status = copy_status(&status);
        span.explicit_status = true;
    }

    fn start_child_span<'a>(
        &'a self,
        options: SpanOptions,
        callback: ErasedSpanCallback<'a>,
    ) -> ErasedSpanFuture<'a> {
        let state = self.state.clone();
        let index = self.index;
        Box::pin(async move { start_in_memory_span(state, Some(index), options, callback).await })
    }
}

/// 对应 `InMemoryTelemetryContext`。
///
/// 后端中立的参考实现，把 span 记录在进程内存中。
/// 创建新实例以隔离测试或独立的记录作用域。
pub struct InMemoryTelemetryContext {
    state: Arc<Mutex<InMemoryTelemetryState>>,
}

impl Default for InMemoryTelemetryContext {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryTelemetryState::new())),
        }
    }
}

impl InMemoryTelemetryContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `getSpans()`：返回按 span 启动顺序排列的分离快照。
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        let st = self.state.lock().unwrap();
        st.spans
            .iter()
            .map(|span| RecordedTelemetrySpan {
                id: span.id,
                parent_id: span.parent_id,
                name: span.name.clone(),
                attributes: copy_attributes(Some(&span.attributes)),
                events: span
                    .events
                    .iter()
                    .map(|event| RecordedTelemetryEvent {
                        name: event.name.clone(),
                        attributes: copy_attributes(Some(&event.attributes)),
                    })
                    .collect(),
                status: copy_status(&span.status),
                settled: span.settled,
                end_sequence: span.end_sequence,
            })
            .collect()
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn start_span<'a, F, Fut, T>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
        T: Send + 'a,
    {
        let state = self.state.clone();
        Box::pin(async move { start_in_memory_span(state, None, options, callback).await })
    }
}
