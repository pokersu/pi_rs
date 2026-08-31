//! Rust 翻译自 packages/telemetry/src/index.ts
//!
//! Vendor-neutral telemetry contracts and typed schema utilities for pi.

mod memory;
mod noop;
pub mod testing;

pub use memory::{InMemoryTelemetryContext, RecordedTelemetryEvent, RecordedTelemetrySpan};
pub use noop::NOOP_TELEMETRY_CONTEXT;

use std::any::Any;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 对应 `AttributeValue = string | number | boolean | string[] | number[] | boolean[]`
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Boolean(bool),
    StringArray(Vec<String>),
    NumberArray(Vec<f64>),
    BooleanArray(Vec<bool>),
}

/// 对应 `SpanAttributes = { [name: string]: AttributeValue | undefined }`。
/// `Some(None)` 对应 `undefined`（复制时被跳过）。
pub type SpanAttributes = BTreeMap<String, Option<AttributeValue>>;

/// 对应 `SpanOptions`
#[derive(Debug, Clone, Default)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: Option<SpanAttributes>,
}

/// 对应 `SpanStatus`
#[derive(Debug, Clone, PartialEq)]
pub enum SpanStatus {
    Ok,
    Error { error: Option<SpanError> },
}

/// 对应 `SpanStatus` error 分支的 `{ name: string; message: string }`
#[derive(Debug, Clone, PartialEq)]
pub struct SpanError {
    pub name: String,
    pub message: String,
}

/// 类型擦除的 span 回调，用于 span 上的递归 `start_child_span`。
pub type ErasedSpanCallback<'a> = Box<
    dyn FnOnce(
            Arc<dyn TelemetrySpan>,
        ) -> Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send + 'a>>
        + Send
        + 'a,
>;

/// 类型擦除的 span 返回值 future。
pub type ErasedSpanFuture<'a> = Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send + 'a>>;

/// 对应 `TelemetryContext`。
///
/// TS 原版 `startSpan<T>` 为接口上的泛型方法；Rust 中该 trait 不 dyn 兼容
/// （含泛型方法），由具体类型或泛型参数使用，等价于 TS 的 `Promise<T>` 返回。
pub trait TelemetryContext: Send + Sync {
    fn start_span<'a, F, Fut, T>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
        T: Send + 'a;
}

/// 对应 `TelemetrySpan`。
///
/// TS 原版 `TelemetrySpan extends TelemetryContext`（span 亦含泛型 `startSpan`）。
/// Rust 中为保持 trait 对象（`dyn TelemetrySpan`）可用，将 span 的递归启动
/// 拆为类型擦除的 `start_child_span`，而顶层泛型 `start_span` 保留在
/// `TelemetryContext` 上。
pub trait TelemetrySpan: Send + Sync {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);

    /// 对应 TS 中 `TelemetrySpan` 从 `TelemetryContext` 继承的 `startSpan`，
    /// 用于在 span 上递归创建子 span；返回值用 `Box<dyn Any + Send>` 擦除。
    fn start_child_span<'a>(
        &'a self,
        options: SpanOptions,
        callback: ErasedSpanCallback<'a>,
    ) -> ErasedSpanFuture<'a>;
}

/// 对应 `TelemetryAttributeType`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryAttributeType {
    String,
    Number,
    Boolean,
    StringArray,
    NumberArray,
    BooleanArray,
}

/// 对应 `TelemetryAttributeMetadata`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TelemetryAttributeMetadata {
    pub description: String,
    pub sensitive: Option<bool>,
    pub cardinality: Option<Cardinality>,
}

/// 对应 `cardinality?: "low" | "high"`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    Low,
    High,
}

/// 对应 `TelemetryAttributeDefinition`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryAttributeDefinition {
    pub metadata: TelemetryAttributeMetadata,
    pub kind: TelemetryAttributeKind,
}

/// 对应 `TelemetryAttributeDefinition` 的判别联合体。
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryAttributeKind {
    String {
        values: Option<Vec<String>>,
        examples: Option<Vec<String>>,
    },
    Number {
        values: Option<Vec<f64>>,
        examples: Option<Vec<f64>>,
    },
    Boolean {
        values: Option<Vec<bool>>,
        examples: Option<Vec<bool>>,
    },
    StringArray {
        element_values: Option<Vec<String>>,
        examples: Option<Vec<Vec<String>>>,
    },
    NumberArray {
        element_values: Option<Vec<f64>>,
        examples: Option<Vec<Vec<f64>>>,
    },
    BooleanArray {
        element_values: Option<Vec<bool>>,
        examples: Option<Vec<Vec<bool>>>,
    },
}

/// 对应 `TelemetryStartAttributeDefinition = TelemetryAttributeDefinition & { required: boolean }`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryStartAttributeDefinition {
    pub definition: TelemetryAttributeDefinition,
    pub required: bool,
}

/// 对应 `TelemetryEventAttributeDefinition = TelemetryAttributeDefinition & { required: boolean }`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryEventAttributeDefinition {
    pub definition: TelemetryAttributeDefinition,
    pub required: bool,
}

/// 对应 `TelemetryEventDefinition`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryEventDefinition {
    pub description: String,
    pub attributes: BTreeMap<String, TelemetryEventAttributeDefinition>,
}

/// 对应 `TelemetryParentDefinition`
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryParentDefinition {
    Any,
    RootOrExternal,
    Spans { spans: Vec<String> },
}

/// 对应 `TelemetrySpanDefinition`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySpanDefinition {
    pub description: String,
    pub parents: TelemetryParentDefinition,
    pub start_attributes: BTreeMap<String, TelemetryStartAttributeDefinition>,
    pub end_attributes: BTreeMap<String, TelemetryAttributeDefinition>,
    pub events: Option<BTreeMap<String, TelemetryEventDefinition>>,
    pub status: TelemetrySpanStatus,
}

/// 对应 `TelemetrySpanDefinition.status: { default: "ok"; errorWhen: string }`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySpanStatus {
    pub default: SpanDefaultStatus,
    pub error_when: String,
}

/// 对应 `default: "ok"`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanDefaultStatus {
    Ok,
}

/// 对应 `TelemetrySchemaDefinition`
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySchemaDefinition {
    pub version: u32,
    pub spans: BTreeMap<String, TelemetrySpanDefinition>,
}

/// 对应 `defineTelemetrySchema`。
/// TS 中该函数借助 `const` 类型参数保留 schema 的字面量类型以供类型推断；
/// Rust 无此机制，退化为恒等函数。
pub fn define_telemetry_schema(schema: TelemetrySchemaDefinition) -> TelemetrySchemaDefinition {
    schema
}

/// 对应 TS 的 `TypedSpanStarter`。
///
/// TS 中它是按 span 名重载、依赖类型体操的类型级函数；Rust 无法表达该映射，
/// 退化为绑定到单个父上下文的 span 启动器（泛型 `C`），运行时行为一致。
pub struct TypedSpanStarter<C: TelemetryContext> {
    context: Arc<C>,
}

impl<C: TelemetryContext> TypedSpanStarter<C> {
    pub fn start_span<'a, F, Fut, T>(
        &'a self,
        name: &str,
        attributes: SpanAttributes,
        callback: F,
    ) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
        T: Send + 'a,
    {
        self.context.start_span(
            SpanOptions {
                name: name.to_string(),
                attributes: Some(attributes),
            },
            callback,
        )
    }
}

/// 对应 TS 的 `bindTypedSpanStarter`。
fn bind_typed_span_starter<C: TelemetryContext>(telemetry_context: Arc<C>) -> TypedSpanStarter<C> {
    TypedSpanStarter {
        context: telemetry_context,
    }
}

/// 对应 `createTypedSpanStarter`。
/// TS 中 schema 仅用于编译期类型推断、运行时不做校验；Rust 保留签名但忽略 `_schemas`。
pub fn create_typed_span_starter<C: TelemetryContext>(
    telemetry_context: Arc<C>,
    _schemas: &[TelemetrySchemaDefinition],
) -> TypedSpanStarter<C> {
    bind_typed_span_starter(telemetry_context)
}
