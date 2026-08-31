//! Rust 翻译自 telemetry 的 conformance 测试（验证 memory 参考实现满足契约）。

use std::sync::Arc;

use pi_telemetry::InMemoryTelemetryContext;
use pi_telemetry::testing::create_telemetry_adapter_conformance;

fn fixture_factory() -> pi_telemetry::testing::TelemetryAdapterFixture {
    pi_telemetry::testing::TelemetryAdapterFixture {
        context: Arc::new(InMemoryTelemetryContext::new()),
    }
}

#[tokio::test]
async fn in_memory_context_satisfies_conformance() {
    let cases = create_telemetry_adapter_conformance(Arc::new(fixture_factory));
    assert!(!cases.is_empty());
    for case in cases {
        case.run().await;
    }
}
