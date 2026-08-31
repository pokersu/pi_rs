//! Rust 翻译自 packages/telemetry/src/testing/types.ts

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::{InMemoryTelemetryContext, RecordedTelemetrySpan};

/// 对应 `TelemetryAdapterFixture`：一个 conformance case 拥有的独立 adapter 实例
/// 和标准化的快照读取器。
///
/// TS 中 `context` 为抽象接口 `TelemetryContext`；因 Rust 的 trait 对象无法调用
/// 泛型 `start_span`，此处固定为参考实现 `InMemoryTelemetryContext`。
/// TS 的 `AsyncDisposable` 在 Rust 中无对应资源需要释放，故省略。
pub struct TelemetryAdapterFixture {
    pub context: Arc<InMemoryTelemetryContext>,
}

impl TelemetryAdapterFixture {
    /// 对应 `getSpans()`
    pub async fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        self.context.get_spans()
    }
}

/// 对应 `TelemetryAdapterFixtureFactory`。
/// TS 中为 `() => Promise<Fixture>`；Rust 中 `InMemoryTelemetryContext` 构造为同步。
pub type TelemetryAdapterFixtureFactory = Arc<dyn Fn() -> TelemetryAdapterFixture + Send + Sync>;

/// 对应 `TelemetryAdapterConformanceCase`：可注册到任意测试框架的、与运行器无关的
/// conformance case。
pub struct TelemetryAdapterConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    pub(crate) run: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>,
}

impl TelemetryAdapterConformanceCase {
    /// 对应 `run()`
    pub async fn run(self) {
        (self.run)().await
    }
}
